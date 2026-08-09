//! An Elecraft radio on a cable, with no daemon in between.
//!
//! The KX3 and the K3 speak Elecraft's own command set: ASCII, every command
//! and every answer ended with a semicolon, over a serial port at whatever rate
//! the radio's menu is set to (38400 out of the box on a KX3). The handful used
//! here is the whole of what this app wants from a radio:
//!
//! | | |
//! |---|---|
//! | `TX;` / `RX;` | key and unkey |
//! | `FA;` | VFO A, eleven digits of hertz |
//! | `FA00014074000;` | tune VFO A |
//! | `MD;` | mode, as a digit |
//! | `AI0;` | stop volunteering: this is a question-and-answer link |
//!
//! `AI0` matters more than it looks. With auto-information on, the radio sends
//! updates nobody asked for — a knob turned, a mode changed — and a client that
//! reads one answer per command reads those instead, one command out of step
//! for ever after. It is sent once when the port opens.
//!
//! Why this rather than `rigctld`, which already speaks to a KX3: because it is
//! one cable and no daemon, and because the protocol is small enough that
//! supporting it directly costs less than explaining how to run hamlib. The
//! daemon stays for every other radio.

use std::io::{Read, Write};

use super::{Fault, Transmitter, KEYING, READING_DIAL, READING_PTT, SETTING_MODE, TUNING};

/// The modes an Elecraft will answer to, in the words this app shows.
///
/// Elecraft numbers them on the wire; these are the names, and `DATA` is the
/// one a station working these modes lives in.
pub const MODES: &[&str] = &["DATA", "DATA-REV", "USB", "LSB", "CW", "CW-REV", "AM", "FM"];

/// What a KX3 is set to when it leaves Elecraft.
pub const KX3_BAUD: u32 = 38400;

/// An Elecraft radio, over anything that carries bytes.
///
/// Generic over the link so the protocol can be tested without a radio or a
/// serial port: everything below the `;` is exercised against a pair of buffers.
pub struct Elecraft<L> {
    link: L,
    what: String,
    /// Cleared once the radio has been told to stop volunteering.
    greeted: bool,
}

impl<L: Read + Write + Send> Elecraft<L> {
    pub fn new(link: L, what: String) -> Elecraft<L> {
        Elecraft { link, what, greeted: false }
    }

    /// Send a command and read its answer, up to and including the `;`.
    ///
    /// Commands that set something answer nothing at all, so only the ones that
    /// ask are read back — a read after `TX;` would sit until the timeout and
    /// then report a radio that is working perfectly as one that is not
    /// answering.
    fn ask(&mut self, command: &str) -> Result<String, Fault> {
        self.greet()?;
        self.write(command)?;
        self.read_answer(command)
    }

    fn tell(&mut self, command: &str) -> Result<(), Fault> {
        self.greet()?;
        self.write(command)
    }

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
        // Elecraft's longest answer is `IF`, at 38 characters; the bound is
        // generous and only exists so that a radio babbling cannot hold this
        // thread for ever.
        while answer.len() < 128 {
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

/// The eleven digits of hertz an `FA` answer carries.
fn frequency_in(answer: &str) -> Option<f64> {
    let digits = answer.strip_prefix("FA")?;
    if digits.len() < 11 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<f64>().ok()
}

/// What the radio calls the mode it is in.
///
/// Elecraft numbers them, and `DATA` is the one this app is usually looking at:
/// a station working digital modes is in `DATA` or `DATA-REV`, and calling that
/// "6" on screen would help nobody.
fn mode_name(digit: char) -> &'static str {
    match digit {
        '1' => "LSB",
        '2' => "USB",
        '3' => "CW",
        '4' => "FM",
        '5' => "AM",
        '6' => "DATA",
        '7' => "CW-REV",
        '9' => "DATA-REV",
        _ => "?",
    }
}

/// The digit Elecraft wants for a mode this app names.
pub(super) fn mode_digit(name: &str) -> Option<char> {
    Some(match name {
        "LSB" => '1',
        "USB" => '2',
        "CW" => '3',
        "FM" => '4',
        "AM" => '5',
        "DATA" => '6',
        "CW-REV" => '7',
        "DATA-REV" => '9',
        _ => return None,
    })
}

impl<L: Read + Write + Send> Transmitter for Elecraft<L> {
    fn key(&mut self, on: bool) -> Result<(), Fault> {
        // Neither is answered, so neither is read back. The rig is asked what
        // it is doing separately, which is the only honest way to know.
        self.tell(if on { "TX;" } else { "RX;" }).map_err(|e| match e {
            Fault::Link(m) => Fault::Link(format!("{KEYING}: {m}")),
            other => other,
        })
    }

    /// Not yet: this radio is told, not asked.
    ///
    /// Elecraft carries the transmit state inside the `IF` record, whose exact
    /// layout wants checking against a radio before anything is read out of it.
    /// `None` means "cannot be asked", which the interface already draws.
    fn keyed(&mut self) -> Result<Option<bool>, Fault> {
        let _ = READING_PTT;
        Ok(None)
    }

    fn dial_hz(&mut self) -> Result<Option<f64>, Fault> {
        let answer = self.ask("FA;")?;
        match frequency_in(&answer) {
            Some(hz) => Ok(Some(hz)),
            None => Err(Fault::Protocol(format!(
                "{READING_DIAL}: expected an FA record, got {answer:?}"
            ))),
        }
    }

    fn tune(&mut self, hz: f64) -> Result<(), Fault> {
        let hz = hz.max(0.0).round() as u64;
        if hz > 99_999_999_999 {
            return Err(Fault::Rejected { doing: TUNING, code: -1 });
        }
        self.tell(&format!("FA{hz:011};"))
    }

    fn dial_mode(&mut self) -> Result<Option<String>, Fault> {
        let answer = self.ask("MD;")?;
        match answer.strip_prefix("MD").and_then(|d| d.chars().next()) {
            Some(digit) => Ok(Some(mode_name(digit).to_string())),
            None => Err(Fault::Protocol(format!("expected an MD record, got {answer:?}"))),
        }
    }

    fn set_mode(&mut self, mode: &str) -> Result<(), Fault> {
        // A word this radio has no digit for is refused rather than sent: `MD;`
        // with nonsense after it is a command the radio ignores silently, which
        // would read here as a mode change that worked and did nothing.
        let Some(digit) = mode_digit(mode) else {
            return Err(Fault::Rejected { doing: SETTING_MODE, code: -1 });
        };
        self.tell(&format!("MD{digit};"))
    }

    fn describe(&self) -> String {
        format!("Elecraft on {}", self.what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A radio made of two buffers: what it was told, and what it will say.
    #[derive(Clone, Default)]
    struct Bench {
        heard: Arc<Mutex<Vec<u8>>>,
        says: Arc<Mutex<Vec<u8>>>,
    }

    impl Bench {
        fn answering(answer: &str) -> Bench {
            let b = Bench::default();
            b.says.lock().unwrap().extend_from_slice(answer.as_bytes());
            b
        }
        fn heard(&self) -> String {
            String::from_utf8(self.heard.lock().unwrap().clone()).unwrap()
        }
        fn rig(&self) -> Elecraft<Bench> {
            Elecraft::new(self.clone(), "a bench".to_string())
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

    /// Keying is two commands and no answer, and the radio is told to stop
    /// volunteering before anything else is said to it.
    #[test]
    fn keying_says_tx_and_rx_and_waits_for_nothing() {
        let bench = Bench::default();
        let mut rig = bench.rig();
        rig.key(true).expect("could not key");
        rig.key(false).expect("could not unkey");
        assert_eq!(bench.heard(), "AI0;TX;RX;");
    }

    /// With auto-information left on, a radio volunteers what nobody asked for
    /// and every answer after it belongs to the command before.
    #[test]
    fn the_radio_is_told_to_stop_volunteering_once() {
        let bench = Bench::answering("FA00014074000;FA00014074000;");
        let mut rig = bench.rig();
        rig.dial_hz().expect("no frequency");
        rig.dial_hz().expect("no frequency");
        assert_eq!(bench.heard(), "AI0;FA;FA;", "greeted more than once, or not at all");
    }

    #[test]
    fn the_frequency_reads_back_in_hertz() {
        let bench = Bench::answering("FA00014074000;");
        assert_eq!(bench.rig().dial_hz(), Ok(Some(14_074_000.0)));

        // Eleven digits, zero padded, which is what the radio wants back.
        let bench = Bench::default();
        bench.rig().tune(7_078_000.0).expect("could not tune");
        assert_eq!(bench.heard(), "AI0;FA00007078000;");
    }

    #[test]
    fn the_mode_comes_back_as_a_word() {
        for (digit, name) in [('2', "USB"), ('6', "DATA"), ('9', "DATA-REV"), ('3', "CW")] {
            let bench = Bench::answering(&format!("MD{digit};"));
            assert_eq!(bench.rig().dial_mode(), Ok(Some(name.to_string())), "MD{digit}");
        }
    }

    /// Setting a mode is the digit the radio wants, and a word it has no digit
    /// for is refused rather than sent — `MD` with nonsense after it is a
    /// command the radio ignores in silence, which would read here as a change
    /// that worked and did nothing.
    #[test]
    fn the_mode_goes_out_as_the_radios_own_digit() {
        let bench = Bench::default();
        bench.rig().set_mode("DATA").expect("could not set the mode");
        assert_eq!(bench.heard(), "AI0;MD6;");

        let bench = Bench::default();
        assert!(matches!(
            bench.rig().set_mode("PKTUSB"),
            Err(Fault::Rejected { doing: SETTING_MODE, .. })
        ));
        // Nothing at all: the word is refused before the port is even greeted.
        assert_eq!(bench.heard(), "", "sent a mode the radio has no digit for");
    }

    /// A radio that says nothing is a fault, not an empty answer.
    #[test]
    fn a_silent_radio_times_out() {
        let bench = Bench::default();
        assert_eq!(bench.rig().dial_hz(), Err(Fault::Timeout));
    }

    /// An answer to the wrong question is reported rather than parsed.
    #[test]
    fn an_answer_that_is_not_the_one_asked_for_is_refused() {
        let bench = Bench::answering("MD6;");
        assert!(matches!(bench.rig().dial_hz(), Err(Fault::Protocol(_))));
    }

}
