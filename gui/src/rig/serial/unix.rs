//! The `termios` half: Linux, macOS and the BSDs.
//!
//! One source for all of them. `termios` is POSIX and the parts a radio needs —
//! raw mode, a baud rate, eight bits, a read that gives up — are spelled the
//! same everywhere; the only real divergence is where the device nodes live and
//! what they are called, which is [`candidates`]'s problem rather than
//! [`open`]'s.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::Candidate;

/// Where a USB-serial adapter usually lands. `ttyUSB0` is the first one, and on
/// a machine with one radio it is nearly always right.
#[cfg(not(target_os = "macos"))]
pub const DEFAULT_DEVICE: &str = "/dev/ttyUSB0";

/// macOS names a serial port after the chip in the cable, with the adapter's
/// serial number stuck on the end — `cu.usbserial-A9014SPX`, `cu.SLAB_USBtoUART`
/// — so there is no constant that is right. This is the stem they share; the
/// picker is what actually finds it.
#[cfg(target_os = "macos")]
pub const DEFAULT_DEVICE: &str = "/dev/cu.usbserial";

/// Open `path` at `baud`: raw, 8N1, no flow control, modem lines down.
pub fn open(path: &str, baud: u32) -> io::Result<File> {
    // `O_NONBLOCK` for the open itself, because a port whose carrier is not
    // asserted blocks there for ever otherwise; cleared immediately after,
    // since the reads below want a timeout rather than a would-block.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)?;
    let fd = file.as_raw_fd();
    configure(fd, baud)?;
    clear_nonblocking(fd)?;
    lower_the_lines(fd)?;
    Ok(file)
}

/// Raw, 8N1, no flow control, and a read that gives up rather than blocking.
fn configure(fd: RawFd, baud: u32) -> io::Result<()> {
    // SAFETY: `fd` is open for the lifetime of the borrow, and `termios` is
    // written whole by `tcgetattr` before it is read.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        t.c_cflag |= libc::CLOCAL | libc::CREAD;
        t.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CRTSCTS);
        t.c_cflag = (t.c_cflag & !libc::CSIZE) | libc::CS8;
        // A read returns what has arrived after this many tenths of a second,
        // or nothing. A radio answers a command in milliseconds; anything that
        // has not answered in half a second is not going to.
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 5;
        let speed = speed_of(baud).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{baud} is not a baud rate"))
        })?;
        if libc::cfsetispeed(&mut t, speed) != 0 || libc::cfsetospeed(&mut t, speed) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
        // Whatever was in the buffers belongs to whoever had the port before.
        libc::tcflush(fd, libc::TCIOFLUSH);
    }
    Ok(())
}

fn clear_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is open, and the flags are read before they are written.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Put DTR and RTS down, in case something is keyed from them.
fn lower_the_lines(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is open; `bits` is written by the ioctl before it is read.
    unsafe {
        let mut bits: libc::c_int = libc::TIOCM_DTR | libc::TIOCM_RTS;
        if libc::ioctl(fd, libc::TIOCMBIC, &mut bits) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The rates a radio is likely to be set to, as `termios` spells them.
fn speed_of(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        1200 => libc::B1200,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115_200 => libc::B115200,
        _ => return None,
    })
}

// ---- finding the ports ----

/// Where udev keeps a stable name for each serial adapter.
///
/// The names there come from the device's own USB descriptor —
/// `usb-Silicon_Labs_CP2102_USB_to_UART_Bridge_Controller_0001-if00-port0` — so
/// they say *which cable this is*, which `ttyUSB0` does not, and they survive
/// the replug that would renumber it. Linux with udev only; macOS has nothing
/// equivalent and simply has no such directory.
const BY_ID: &str = "/dev/serial/by-id";

/// Device-node names that could be a radio cable.
///
/// `ttyUSB` is a USB-serial bridge and `ttyACM` a CDC device — between them,
/// every modern rig interface. `cu.` is macOS's name for a port that opens
/// without waiting for carrier; the same hardware is also `tty.`, which blocks,
/// so only `cu.` belongs in a menu. `ttyS` is an on-board 16550 and is filtered
/// hard — see [`is_real_uart`].
const PREFIXES: &[&str] = &["ttyUSB", "ttyACM", "cu.", "ttyS"];

/// Every serial port this machine appears to have, likeliest first.
pub fn candidates() -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    // The named ones lead, because a name that says "CP2102" is worth more than
    // a name that says "0". The *path* offered is the symlink rather than what
    // it resolves to: that is the one that still means this cable tomorrow.
    if let Some(mut named) = read_dir_sorted(BY_ID) {
        named.sort();
        for link in named {
            let Ok(real) = std::fs::canonicalize(&link) else { continue };
            let node = file_name(&real);
            seen.push(real);
            out.push(Candidate {
                label: format!("{node} — {}", describe(&file_name(&link))),
                path: link.to_string_lossy().into_owned(),
            });
        }
    }

    // Then the raw device nodes, for anything udev did not name — every port on
    // macOS, and on Linux the on-board UARTs, which have no USB descriptor to
    // be named from.
    if let Some(mut nodes) = read_dir_sorted("/dev") {
        nodes.sort();
        for node in nodes {
            let name = file_name(&node);
            if !PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            if name.starts_with("ttyS") && !is_real_uart(&name) {
                continue;
            }
            if is_not_a_radio(&name) {
                continue;
            }
            // Skip what the by-id pass already offered under a better name.
            if std::fs::canonicalize(&node).is_ok_and(|real| seen.contains(&real)) {
                continue;
            }
            out.push(Candidate { path: node.to_string_lossy().into_owned(), label: name });
        }
    }
    out
}

/// Whether `/dev/ttyS<n>` is a UART that exists.
///
/// Linux creates thirty-two of these whether or not the machine has a serial
/// port, and a menu of thirty-two dead entries is worse than no menu. The
/// kernel publishes the chip it found in `/sys`, and reports `0` — `PORT_UNKNOWN`
/// — for the ones that are only nodes. A machine whose `/sys` cannot be read at
/// all keeps none of them: the setting is still typeable, and hiding a port
/// somebody has is a smaller failure than burying the one they want.
fn is_real_uart(name: &str) -> bool {
    let Ok(kind) = std::fs::read_to_string(format!("/sys/class/tty/{name}/type")) else {
        return false;
    };
    kind.trim() != "0"
}

/// Ports macOS advertises that are not a cable to anything.
///
/// A Mac lists its Bluetooth serial profile and the kernel's own debug console
/// alongside real hardware. Neither has a radio on the end.
fn is_not_a_radio(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("bluetooth") || lower.contains("debug-console") || lower.contains("wlan-debug")
}

/// Turn a udev by-id name into something that fits in a menu.
///
/// `usb-Silicon_Labs_CP2102_USB_to_UART_Bridge_Controller_0001-if00-port0`
/// becomes `Silicon Labs CP2102 USB to UART Bridge Controller 0001`. The bus
/// prefix and the interface suffix are the same on everything and say nothing;
/// the serial number in the middle stays, because with two identical adapters
/// plugged in it is the only thing that tells them apart.
fn describe(by_id: &str) -> String {
    let mut s = by_id;
    for prefix in ["usb-", "pci-", "platform-"] {
        s = s.strip_prefix(prefix).unwrap_or(s);
    }
    // `-if00`, `-if00-port0`: the USB interface and its port within it.
    if let Some(cut) = s.find("-if") {
        s = &s[..cut];
    }
    s.replace('_', " ")
}

/// The entries of a directory as paths, or `None` if it cannot be read.
fn read_dir_sorted(dir: &str) -> Option<Vec<PathBuf>> {
    Some(std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect())
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rate an Elecraft or a Yaesu offers is one `termios` knows.
    #[test]
    fn the_rates_a_radio_offers_are_all_known() {
        for baud in [4800, 9600, 19200, 38400] {
            assert!(speed_of(baud).is_some(), "{baud} baud is not supported");
        }
        assert!(speed_of(1_000_000).is_none(), "invented a baud rate");
    }

    /// The long udev name loses its boilerplate and keeps what identifies the
    /// cable, serial number included.
    #[test]
    fn a_by_id_name_becomes_something_readable() {
        assert_eq!(
            describe("usb-Silicon_Labs_CP2102_USB_to_UART_Bridge_Controller_0001-if00-port0"),
            "Silicon Labs CP2102 USB to UART Bridge Controller 0001"
        );
        assert_eq!(describe("usb-FTDI_FT232R_USB_UART_A9014SPX-if00-port0"), "FTDI FT232R USB UART A9014SPX");
        // Two of the same adapter differ only in the serial number, so the
        // serial number is the part that may not be dropped.
        let a = describe("usb-FTDI_FT232R_USB_UART_A50285BI-if00-port0");
        let b = describe("usb-FTDI_FT232R_USB_UART_A9014SPX-if00-port0");
        assert_ne!(a, b, "two adapters described identically");
    }

    /// A name with none of the expected furniture survives it unharmed.
    #[test]
    fn an_unfamiliar_name_is_left_alone() {
        assert_eq!(describe("something-else"), "something-else");
    }

    /// The phantom `ttyS` nodes Linux creates for absent hardware stay out of
    /// the menu. This machine may or may not have a real one; what it certainly
    /// has is thirty-two nodes, and they may not all come through.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_menu_is_not_thirty_two_dead_serial_ports() {
        let listed = candidates().iter().filter(|c| c.path.starts_with("/dev/ttyS")).count();
        assert!(listed < 4, "listed {listed} on-board UARTs, which is more than any machine has");
    }

    /// What macOS lists that is not a radio does not reach the menu.
    #[test]
    fn the_bluetooth_port_is_not_offered() {
        assert!(is_not_a_radio("cu.Bluetooth-Incoming-Port"));
        assert!(is_not_a_radio("cu.debug-console"));
        assert!(!is_not_a_radio("cu.usbserial-A9014SPX"));
    }
}
