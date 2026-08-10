//! A serial port, opened raw.
//!
//! Enough of the platform's terminal API to talk to a radio and no more: eight
//! bits, no parity, one stop bit, no flow control, no line discipline getting
//! between the bytes and the wire. A rig's CAT link is a stream of characters
//! and everything the terminal driver would helpfully do to them — echo,
//! newline translation, signal characters — is damage.
//!
//! The lines are left alone on the way in. Opening a port raises DTR and RTS by
//! default, and on an interface wired to key from them that *is* a
//! transmission: the radio goes on the air because somebody started a program.
//! [`Port::open`] puts both down as the first thing it does after the open, on
//! every platform. It cannot make the open itself not raise them — that happens
//! inside the driver — so the window is a few microseconds wide rather than
//! nothing, which is the best either API offers.
//!
//! Two implementations sit behind this. [`unix`] is `termios`, which covers
//! Linux, macOS and the BSDs from the same source; [`windows`] is `DCB` and
//! `COMMTIMEOUTS`. Both hand back a plain [`File`], so everything above this
//! line — reading, writing, describing, the whole of `elecraft` — is written
//! once and is the same code everywhere.

use std::fs::File;
use std::io::{self, Read, Write};

#[cfg_attr(docsrs, doc(cfg(unix)))]
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;

#[cfg_attr(docsrs, doc(cfg(windows)))]
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

/// An open serial port.
pub struct Port {
    file: File,
    what: String,
}

impl Port {
    /// Open `path` at `baud`, raw, with the modem lines down.
    pub fn open(path: &str, baud: u32) -> io::Result<Port> {
        let file = sys::open(path, baud)?;
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

/// A port the machine says it has, as something to offer the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What to hand [`Port::open`], and what to save in the settings.
    pub path: String,
    /// What to show in a menu: short, and saying which cable this is where the
    /// machine knows.
    pub label: String,
}

/// Every serial port this machine appears to have, likeliest first.
///
/// A guess, and offered as one. Asking the operating system what ports exist is
/// answered differently on each platform and completely on none of them: a USB
/// adapter that enumerated after this was called is missing, and a port listed
/// here may still be held open by something else. The setting stays typeable
/// for exactly that reason — this is a convenience, not the interface.
pub fn ports() -> Vec<Candidate> {
    sys::candidates()
}

/// What to start from before anything is detected or saved.
///
/// A starting point rather than an answer: the first port [`ports`] finds is a
/// far better guess than any constant, and this is what shows when it finds
/// nothing at all. Kept because the settings need *something* the first time
/// they are drawn.
pub const DEFAULT_DEVICE: &str = sys::DEFAULT_DEVICE;

#[cfg(test)]
mod tests {
    use super::*;

    /// A device that is not there fails as a device that is not there, rather
    /// than as something the operator has to interpret.
    #[test]
    fn a_port_that_does_not_exist_says_so() {
        // Not a path that could be a real port on either platform: a Unix
        // device node lives under `/dev`, and a Windows port is `COM<n>`.
        let Err(e) = Port::open("definitely-not-a-serial-port", 38400) else {
            panic!("opened a port that does not exist");
        };
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    /// Whatever is listed can be opened by the thing that listed it — a menu
    /// entry whose path needed further interpretation would be a trap.
    #[test]
    fn every_listed_port_is_named_by_its_own_path() {
        for c in ports() {
            assert!(!c.path.is_empty(), "a port with no path: {c:?}");
            assert!(!c.label.is_empty(), "a port with no label: {c:?}");
        }
    }

    /// The fallback is a plausible port name on the platform it is compiled
    /// for, since it is what the operator is shown before anything is detected.
    #[test]
    fn the_default_device_looks_like_a_port() {
        if cfg!(windows) {
            assert!(DEFAULT_DEVICE.starts_with("COM"), "{DEFAULT_DEVICE} is not a COM port");
        } else {
            assert!(DEFAULT_DEVICE.starts_with("/dev/"), "{DEFAULT_DEVICE} is not a device node");
        }
    }
}
