//! Semicolon-terminated ASCII CAT, which several makes of radio speak.
//!
//! Elecraft and the current Yaesus use the same shape of link even though they
//! disagree about nearly every command in it: printable ASCII, each command and
//! each answer ended with a `;`, over a serial port at whatever rate the
//! radio's menu is set to. What is here is that shape and nothing about any
//! particular radio — the vocabulary lives in [`super::elecraft`] and
//! [`super::yaesu`].
//!
//! It is shared rather than copied because the subtle part is shared. Reading
//! an answer is not "read until `;`": a radio volunteers records nobody asked
//! for, and taking answers in order puts the link one command out of step for
//! ever after. That bug was found on a KX3 mid-tune, and one copy of the fix is
//! all anybody should have to maintain.

use std::io::{Read, Write};

use super::Fault;

/// A radio on the other end of a CAT link.
///
/// Generic over the link so a protocol can be tested without a radio or a
/// serial port: everything below the `;` is exercised against a pair of
/// buffers.
pub struct Cat<L> {
    link: L,
    /// Cleared once the radio has been told to stop volunteering.
    greeted: bool,
}

/// Longest answer to read before deciding the radio is not speaking this.
///
/// Elecraft's longest is `IF` at 38 characters and a Yaesu's is shorter. The
/// bound is generous and only exists so that a radio babbling cannot hold the
/// thread for ever.
const LONGEST_ANSWER: usize = 128;

/// How many records for other commands to step over before giving up.
///
/// Past a handful the link is not one this can use, and saying so beats reading
/// for ever.
const PATIENCE: usize = 8;

impl<L: Read + Write + Send> Cat<L> {
    pub fn new(link: L) -> Cat<L> {
        Cat { link, greeted: false }
    }

    /// Ask a question and read *its* answer.
    ///
    /// Records are matched to the question by their leading characters rather
    /// than taken in order, and anything else that turns up is discarded. A
    /// radio volunteers: `AI0;` is meant to stop it, and on a KX3 being tuned
    /// by hand it does not entirely — each change of frequency can produce an
    /// `FA` nobody asked for. Read in order, one of those puts every answer
    /// after it one command out of step, and what comes back is the tail of the
    /// record before: a frequency that is somebody's previous reading, then
    /// eventually a partial record that parses as nothing at all.
    ///
    /// Which is what "the rig answered nonsense" was, in the middle of tuning
    /// from 14 MHz to 7.
    ///
    /// The question without its `;` is the prefix an answer has to carry, which
    /// works for both vocabularies here: `FA;` is answered by `FA…`, and a
    /// Yaesu's `MD0;` by `MD0…`.
    ///
    /// Commands that *set* something are answered by nothing at all, so they do
    /// not come through here — a read after `TX;` would wait out the timeout
    /// and report a radio that is working perfectly as one that is not.
    pub fn ask(&mut self, query: &str) -> Result<String, Fault> {
        self.greet()?;
        self.write(query)?;
        let want = query.trim_end_matches(';');
        for _ in 0..PATIENCE {
            let answer = self.read_answer(query)?;
            if answer.starts_with(want) {
                return Ok(answer);
            }
        }
        Err(Fault::Protocol(format!("{query:?} was answered with records for other commands")))
    }

    /// Say something the radio does not answer.
    pub fn tell(&mut self, command: &str) -> Result<(), Fault> {
        self.greet()?;
        self.write(command)
    }

    /// Stop the radio volunteering, once, before anything else is said to it.
    ///
    /// `AI0;` is the same command on both makes, which is the one piece of
    /// vocabulary this layer is willing to know: it is not about what the radio
    /// is, it is about the link being question-and-answer.
    fn greet(&mut self) -> Result<(), Fault> {
        if !self.greeted {
            self.greeted = true;
            self.write("AI0;")?;
        }
        Ok(())
    }

    fn write(&mut self, command: &str) -> Result<(), Fault> {
        self.link
            .write_all(command.as_bytes())
            .and_then(|()| self.link.flush())
            .map_err(|e| Fault::Link(format!("sending {command:?}: {e}")))
    }

    fn read_answer(&mut self, command: &str) -> Result<String, Fault> {
        let mut answer = String::new();
        let mut byte = [0u8; 1];
        while answer.len() < LONGEST_ANSWER {
            match self.link.read(&mut byte) {
                // A configured port returns nothing when its read timer
                // expires, which is the shape a silent radio has.
                Ok(0) => return Err(Fault::Timeout),
                Ok(_) => {
                    let c = byte[0] as char;
                    if c == ';' {
                        return Ok(answer);
                    }
                    // Anything before the answer proper — a stray newline, the
                    // tail of something the radio said unprompted — is not part
                    // of it.
                    if !c.is_control() {
                        answer.push(c);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Err(Fault::Timeout),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Fault::Link(format!("reading the answer to {command:?}: {e}"))),
            }
        }
        Err(Fault::Protocol(format!("{command:?} was answered at length and without a `;`")))
    }
}

#[cfg(test)]
pub(super) mod bench {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A radio made of two buffers: what it was told, and what it will say.
    ///
    /// Shared by both drivers' tests, since a radio that is only a pair of
    /// buffers is not an Elecraft or a Yaesu.
    #[derive(Clone, Default)]
    pub(in crate::rig) struct Bench {
        heard: Arc<Mutex<Vec<u8>>>,
        says: Arc<Mutex<Vec<u8>>>,
    }

    impl Bench {
        /// A radio with something already queued to say.
        pub(in crate::rig) fn answering(answer: &str) -> Bench {
            let b = Bench::default();
            b.says.lock().unwrap().extend_from_slice(answer.as_bytes());
            b
        }

        /// Everything said to the radio so far.
        pub(in crate::rig) fn heard(&self) -> String {
            String::from_utf8(self.heard.lock().unwrap().clone()).unwrap()
        }
    }

    impl Read for Bench {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut says = self.says.lock().unwrap();
            if says.is_empty() {
                return Ok(0); // a silent radio, as a timed-out port reads
            }
            let n = buf.len().min(says.len());
            buf[..n].copy_from_slice(&says[..n]);
            says.drain(..n);
            Ok(n)
        }
    }

    impl Write for Bench {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.heard.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
