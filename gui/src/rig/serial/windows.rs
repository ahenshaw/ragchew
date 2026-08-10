//! The Win32 half.
//!
//! `DCB` where Unix has `termios`, `COMMTIMEOUTS` where it has `VMIN`/`VTIME`,
//! and the registry where it has `/dev`. The settings arrived at are the same
//! ones: raw eight-bit characters, no flow control, no line the driver may
//! assert on its own, and a read that returns what has come rather than waiting
//! for a length it will never reach.
//!
//! The handle is handed straight to [`File`] once it is configured, so reading
//! and writing are ordinary `std` and the port is closed when it is dropped.

use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;

use windows_sys::Win32::Devices::Communication::{
    CLRDTR, CLRRTS, COMMTIMEOUTS, DCB, EscapeCommFunction, GetCommState, NOPARITY, ONESTOPBIT,
    PURGE_RXCLEAR, PURGE_TXCLEAR, PurgeComm, SetCommState, SetCommTimeouts,
};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
};

use super::Candidate;

/// A machine with one USB adapter and nothing else tends to land here: COM1 and
/// COM2 are conventionally the on-board ports, whether or not the machine has
/// any. Only a starting point — the picker is what finds the real one.
pub const DEFAULT_DEVICE: &str = "COM3";

/// Open `path` at `baud`: raw, 8N1, no flow control, modem lines down.
pub fn open(path: &str, baud: u32) -> io::Result<File> {
    let name = wide(&device_path(path));
    // No sharing: two programs keying the same radio is not a thing to support
    // by accident. `OPEN_EXISTING` because a COM port is never created, and no
    // `FILE_FLAG_OVERLAPPED` because the reads here want to block up to a
    // timeout, which is what a synchronous handle does.
    //
    // SAFETY: `name` is NUL-terminated and outlives the call; the two null
    // pointers are the documented "no security attributes, no template".
    let handle: HANDLE = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // From here the handle is owned: `File` closes it on drop, including on the
    // early returns below.
    //
    // SAFETY: `CreateFileW` returned it, it is not the invalid value, and it is
    // not duplicated anywhere.
    let file = unsafe { File::from_raw_handle(handle) };
    configure(handle, baud)?;
    lower_the_lines(handle)?;
    // Whatever was in the buffers belongs to whoever had the port before.
    //
    // SAFETY: `handle` is open and owned by `file`, which is still alive.
    unsafe { PurgeComm(handle, PURGE_RXCLEAR | PURGE_TXCLEAR) };
    Ok(file)
}

/// `fBinary`, the first bit of the `DCB` flags word.
///
/// Windows requires it set, and everything else in that word — the two
/// flow-control pairs, `fDtrControl`, `fRtsControl`, the error and null
/// substitutions — is wanted off. `DTR_CONTROL_DISABLE` and
/// `RTS_CONTROL_DISABLE` are themselves zero, so clearing the word is both "no
/// flow control" and "do not touch the modem lines" in one assignment. The
/// bitfield is opaque in `windows-sys`, hence the constant rather than a name
/// per flag.
const F_BINARY: u32 = 1;

/// 8N1, no flow control, and a read that gives up rather than blocking.
fn configure(handle: HANDLE, baud: u32) -> io::Result<()> {
    // SAFETY: `handle` is an open comms device; `dcb` is written whole by
    // `GetCommState` before it is read, and `DCBlength` is set as documented.
    unsafe {
        let mut dcb: DCB = std::mem::zeroed();
        dcb.DCBlength = std::mem::size_of::<DCB>() as u32;
        if GetCommState(handle, &mut dcb) == 0 {
            return Err(io::Error::last_os_error());
        }
        dcb.BaudRate = baud;
        dcb.ByteSize = 8;
        dcb.Parity = NOPARITY;
        dcb.StopBits = ONESTOPBIT;
        dcb._bitfield = F_BINARY;
        if SetCommState(handle, &dcb) == 0 {
            return Err(io::Error::last_os_error());
        }

        // The `VMIN = 0`, `VTIME = 5` of the Unix side, spelled the one way
        // Windows spells it. `MAXDWORD` in the interval *and* the multiplier,
        // with a constant, is the documented special case: return the moment
        // any byte has arrived, and after the constant with nothing if none
        // does. A radio answers a command in milliseconds; anything that has
        // not answered in half a second is not going to.
        //
        // The write timeout is a backstop the Unix side has no equivalent for
        // and does not need — a blocking write cannot hang here because nothing
        // is throttling it, but half a second to put twelve characters on the
        // wire is already absurd, so waiting longer only hides a fault.
        const MAXDWORD: u32 = u32::MAX;
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: MAXDWORD,
            ReadTotalTimeoutMultiplier: MAXDWORD,
            ReadTotalTimeoutConstant: 500,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 500,
        };
        if SetCommTimeouts(handle, &timeouts) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Put DTR and RTS down, in case something is keyed from them.
///
/// `configure` has already told the driver never to raise them of its own
/// accord; this drives them low now, for the case where the open left them up.
/// The `TIOCMBIC` of the Unix side, under another name.
fn lower_the_lines(handle: HANDLE) -> io::Result<()> {
    // SAFETY: `handle` is an open comms device.
    unsafe {
        if EscapeCommFunction(handle, CLRDTR) == 0 || EscapeCommFunction(handle, CLRRTS) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The name to open a port by.
///
/// `COM1` through `COM9` are legacy DOS device names and open as written, but
/// `COM10` and up do not: they resolve only through the `\\.\` device
/// namespace. Since the prefixed form works for all of them, everything that
/// looks like a bare COM port gets it. Anything else — an operator who has
/// typed a full `\\.\` path, or a named pipe standing in for a rig — is passed
/// through untouched.
/// The path is whatever was typed into the settings, so it is sliced by
/// [`str::get`] rather than by index: a name three bytes into a multi-byte
/// character would panic on the latter, and a rig setting is not worth taking
/// the interface down over.
fn device_path(path: &str) -> String {
    let number = path
        .get(..3)
        .filter(|head| head.eq_ignore_ascii_case("COM"))
        .and_then(|_| path.get(3..));
    let looks_like_a_com_port =
        number.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if looks_like_a_com_port { format!(r"\\.\{path}") } else { path.to_string() }
}

/// A Rust string as Windows wants it: UTF-16, NUL-terminated.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- finding the ports ----

/// Where Windows lists the serial ports that exist.
///
/// Each value under this key is one port: the name is the driver's device path
/// — `\Device\Serial0`, `\Device\VCP0`, `\Device\USBSER000` — and the data is
/// the `COM<n>` the operator has to type. It is the same list Device Manager
/// draws from, it updates as adapters come and go, and reading it needs no
/// privileges.
const SERIALCOMM: &str = r"HARDWARE\DEVICEMAP\SERIALCOMM";

/// Every serial port this machine appears to have, likeliest first.
pub fn candidates() -> Vec<Candidate> {
    let mut out: Vec<Candidate> = enumerate(SERIALCOMM)
        .into_iter()
        .map(|(device, com)| Candidate { label: format!("{com} — {}", describe(&device)), path: com })
        .collect();
    // `COM10` after `COM9`, not before it: the registry hands these back in the
    // order the drivers registered, which is neither sorted nor stable.
    out.sort_by_key(|c| (number_of(&c.path), c.path.clone()));
    out
}

/// The `COM<n>` in a port name, for sorting. A name that is not one sorts last.
fn number_of(port: &str) -> u32 {
    port.strip_prefix("COM").and_then(|n| n.parse().ok()).unwrap_or(u32::MAX)
}

/// Turn a driver device path into something that fits in a menu.
///
/// `\Device\USBSER000` becomes `USBSER000`. The leading `\Device\` is on all of
/// them and says nothing; what is left is the driver's own name, which is at
/// least the difference between an on-board `Serial0` and a USB `VCP0`. It is
/// less than Unix gets — this key holds no manufacturer or serial number — but
/// it is what is here without walking SetupAPI for it.
fn describe(device: &str) -> String {
    device.rsplit('\\').next().unwrap_or(device).to_string()
}

/// Every value under a registry key, as (name, data) pairs.
///
/// Returns nothing rather than an error for any of it: a missing key means a
/// machine with no serial ports, which is not a fault, and a picker that cannot
/// offer anything is already handled by the setting staying typeable.
fn enumerate(subkey: &str) -> Vec<(String, String)> {
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and outlives the call; `key` is written
    // by the call and only read if it succeeded.
    let opened = unsafe {
        let path = wide(subkey);
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key)
    };
    if opened != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for index in 0.. {
        // Both lengths are in/out: characters for the name, *bytes* for the
        // data, and the call fails rather than truncating if either is short.
        // A device path or a port name that needs more than this is not one.
        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut data = [0u8; 512];
        let mut data_len = data.len() as u32;
        // SAFETY: `key` is open, and every pointer is to a local buffer whose
        // length is passed alongside it.
        let got = unsafe {
            RegEnumValueW(
                key,
                index,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null(),
                std::ptr::null_mut(),
                data.as_mut_ptr(),
                &mut data_len,
            )
        };
        if got != 0 {
            // Out of values, or one that would not fit. Either way this is the
            // end of what can be listed.
            break;
        }
        let device = String::from_utf16_lossy(&name[..name_len as usize]);
        // The data is a UTF-16 string in a byte buffer, NUL included in the
        // length the call reports.
        let wide_data: Vec<u16> = data[..data_len as usize]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let port = String::from_utf16_lossy(&wide_data);
        if !port.is_empty() {
            out.push((device, port));
        }
    }
    // SAFETY: `key` was opened above and is not used after this.
    unsafe { RegCloseKey(key) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ports that need the device namespace get it, and the ones that do
    /// not are not disturbed. `COM10` opening as `COM10` is the classic Windows
    /// serial bug and it is this function's whole job to not have it.
    #[test]
    fn a_two_digit_com_port_gets_the_device_prefix() {
        assert_eq!(device_path("COM1"), r"\\.\COM1");
        assert_eq!(device_path("COM10"), r"\\.\COM10");
        assert_eq!(device_path("com23"), r"\\.\com23");
        // Already a device path, or not a COM port at all.
        assert_eq!(device_path(r"\\.\COM10"), r"\\.\COM10");
        assert_eq!(device_path(r"\\.\pipe\rig"), r"\\.\pipe\rig");
        assert_eq!(device_path("COM"), "COM");
        assert_eq!(device_path("COM3x"), "COM3x");
        // Half-typed and mistyped names arrive here, since the setting is a
        // text field. None of them may take the interface down.
        assert_eq!(device_path(""), "");
        assert_eq!(device_path("CO"), "CO");
        assert_eq!(device_path("ΩΩ1"), "ΩΩ1", "sliced through a character");
    }

    /// A driver device path loses the part that is the same on all of them.
    #[test]
    fn a_device_path_becomes_something_readable() {
        assert_eq!(describe(r"\Device\USBSER000"), "USBSER000");
        assert_eq!(describe(r"\Device\Serial0"), "Serial0");
        assert_eq!(describe("VCP0"), "VCP0");
    }

    /// Ports sort by their number, not by their spelling: a machine with eleven
    /// of them should not read COM1, COM10, COM11, COM2.
    #[test]
    fn ports_sort_the_way_a_person_counts() {
        let mut ports = ["COM10", "COM2", "COM1"];
        ports.sort_by_key(|p| number_of(p));
        assert_eq!(ports, ["COM1", "COM2", "COM10"]);
        assert_eq!(number_of("not a port"), u32::MAX, "an odd name should sort last");
    }

    /// A key that is not there is a machine with no serial ports, not an error.
    #[test]
    fn a_missing_registry_key_lists_nothing() {
        assert!(enumerate(r"HARDWARE\DEVICEMAP\DEFINITELY-NOT-A-KEY").is_empty());
    }
}
