//! A serial port, opened raw.
//!
//! Enough of `termios` to talk to a radio and no more: eight bits, no parity,
//! one stop bit, no flow control, no line discipline getting between the bytes
//! and the wire. A rig's CAT link is a stream of characters and everything the
//! terminal driver would helpfully do to them — echo, newline translation,
//! signal characters — is damage.
//!
//! The lines are left alone on the way in. Opening a port raises DTR and RTS by
//! default, and on an interface wired to key from them that *is* a
//! transmission: the radio goes on the air because somebody started a program.
//! [`Port::open`] lowers both before anything else happens.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};

/// An open serial port.
pub struct Port {
    file: std::fs::File,
    what: String,
}

impl Port {
    /// Open `path` at `baud`, raw, with the modem lines down.
    pub fn open(path: &str, baud: u32) -> io::Result<Port> {
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
        Ok(Port { file, what: format!("{path} at {baud} baud") })
    }

    pub fn describe(&self) -> &str {
        &self.what
    }
}

impl Read for Port {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for Port {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// The `custom_flags` extension is Unix-only, which is where this module is.
use std::os::unix::fs::OpenOptionsExt;

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

    /// A device that is not there fails as a device that is not there, rather
    /// than as something the operator has to interpret.
    #[test]
    fn a_port_that_does_not_exist_says_so() {
        let Err(e) = Port::open("/dev/definitely-not-a-serial-port", 38400) else {
            panic!("opened a port that does not exist");
        };
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }
}
