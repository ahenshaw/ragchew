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

use super::{
    Fault, Transmitter, KEYING, READING_DIAL, READING_PTT, SETTING_MODE, SETTING_POWER,
    SETTING_WIDTH, TUNING,
};

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

    /// Ask a question and read *its* answer.
    ///
    /// Records are matched to the question by their two-letter prefix rather
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
    /// Commands that *set* something are answered by nothing at all, so they do
    /// not come through here — a read after `TX;` would wait out the timeout
    /// and report a radio that is working perfectly as one that is not.
    fn ask(&mut self, query: &str) -> Result<String, Fault> {
        self.greet()?;
        self.write(query)?;
        let want = query.trim_end_matches(';');
        // Bounded, so a radio talking continuously cannot hold this thread:
        // past a handful of unrelated records the link is not one this can use,
        // and saying so beats reading for ever.
        for _ in 0..8 {
            let answer = self.read_answer(query)?;
            if answer.starts_with(want) {
                return Ok(answer);
            }
        }
        Err(Fault::Protocol(format!("{query:?} was answered with records for other commands")))
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

/// Whether an `IF` record says the radio is transmitting.
///
/// `IF` is Elecraft's everything-at-once answer, 38 characters wide, and the
/// transmit flag is one of them. Counted from the end, because everything after
/// the flag is fixed while what comes before it is not: the eleven digits of
/// VFO A are the same width on a KX3 and a K3, but nothing is gained by
/// assuming that when the tail can be counted instead.
///
/// From a KX3, receiving on 14.070:
///
/// ```text
/// IF00014070000     -000000 0002000001 ;
///                                └ transmit flag, ninth from the end
/// ```
///
/// A record that is not the shape this expects gives `None` rather than a
/// guess — the whole reason for asking is to catch a rig keyed by something
/// other than this app, and an indicator reading the wrong column would invent
/// exactly that.
fn transmitting_in(body: &str) -> Option<bool> {
    match body.chars().rev().nth(8) {
        Some('0') => Some(false),
        Some('1') => Some(true),
        _ => None,
    }
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

    fn keyed(&mut self) -> Result<Option<bool>, Fault> {
        let answer = self.ask("IF;")?;
        let Some(body) = answer.strip_prefix("IF") else {
            return Err(Fault::Protocol(format!(
                "{READING_PTT}: expected an IF record, got {answer:?}"
            )));
        };
        // A record this cannot read is one it will not guess at: `None` means
        // "cannot be asked", which the rest of the app already draws.
        Ok(transmitting_in(body))
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

    fn power_w(&mut self) -> Result<Option<f64>, Fault> {
        // `PCnnn;` — watts, three digits.
        let answer = self.ask("PC;")?;
        Ok(answer.strip_prefix("PC").and_then(|d| d.trim().parse::<f64>().ok()))
    }

    fn set_power_w(&mut self, watts: f64) -> Result<(), Fault> {
        let w = watts.round().clamp(0.0, 999.0) as u32;
        let _ = SETTING_POWER;
        self.tell(&format!("PC{w:03};"))
    }

    fn width_hz(&mut self) -> Result<Option<f64>, Fault> {
        // `BWnnnn;` — the filter width in tens of hertz, so 0270 is 2.70 kHz.
        let answer = self.ask("BW;")?;
        Ok(answer
            .strip_prefix("BW")
            .and_then(|d| d.trim().parse::<f64>().ok())
            .map(|tens| tens * 10.0))
    }

    fn set_width_hz(&mut self, hz: f64) -> Result<(), Fault> {
        let tens = (hz / 10.0).round().clamp(0.0, 9999.0) as u32;
        let _ = SETTING_WIDTH;
        self.tell(&format!("BW{tens:04};"))
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

    /// The transmit flag, out of a pair of records a real KX3 sent.
    ///
    /// The one field here read by position rather than by what was sent, so it
    /// is pinned by example and not by a layout I typed out — an earlier
    /// attempt at this checked a parser against a record I had invented myself,
    /// which agreed with itself and proved nothing.
    ///
    /// The pair is what makes it evidence. A receiving record alone cannot tell
    /// the right column from a neighbouring zero; these two differ in exactly
    /// one place outside the frequency, and it is the place this reads.
    #[test]
    fn the_if_record_says_whether_it_is_transmitting() {
        // Captured from a KX3, receiving on 14.070 and transmitting on 14.078.
        let rx = "IF00014070000     -000000 0002000001 ";
        let tx = "IF00014078000     -000000 0012000001 ";
        let bench = Bench::answering(&format!("{rx};"));
        assert_eq!(bench.rig().keyed(), Ok(Some(false)), "read a receiving radio as keyed");
        let bench = Bench::answering(&format!("{tx};"));
        assert_eq!(bench.rig().keyed(), Ok(Some(true)), "read a transmitting radio as receiving");

        // Aligned, not merely different: outside the frequency the two records
        // differ in exactly one column, and it is the one being read. Without
        // this the test would pass just as well on a flag found by luck.
        let (rx_body, tx_body) = (&rx[2..], &tx[2..]);
        let differing: Vec<usize> = rx_body
            .char_indices()
            .zip(tx_body.chars())
            .filter(|((_, a), b)| a != b)
            .map(|((i, _), _)| i)
            .filter(|i| *i >= 11) // the frequency is theirs to differ in
            .collect();
        assert_eq!(differing, [26], "the records differ somewhere unexpected: {differing:?}");
        assert_eq!(rx_body.len() - 1 - 26, 8, "the column is not where this reads it");

        // A record of another shape is not guessed at.
        let bench = Bench::answering("IF123;");
        assert_eq!(bench.rig().keyed(), Ok(None));
        // And a radio that only talks about other things has not answered.
        let bench = Bench::answering("MD6;");
        assert_eq!(bench.rig().keyed(), Err(Fault::Timeout));
    }

    /// Power in watts and the filter in hertz, in the units the radio wants:
    /// `PC` counts watts and `BW` counts tens of hertz, so 2.7 kHz is 0270.
    ///
    /// Confirmed against a KX3: five watts and 2.7 kHz set from the app read
    /// back as five watts and 2.7 kHz on the radio's own display. Worth having
    /// checked — the units are exactly the kind of thing to get wrong, and a
    /// filter set ten times too wide is a receiver listening to the whole
    /// band.
    #[test]
    fn power_and_filter_go_out_in_the_radios_own_units() {
        let bench = Bench::default();
        bench.rig().set_power_w(10.0).expect("could not set power");
        assert_eq!(bench.heard(), "AI0;PC010;");

        let bench = Bench::answering("PC005;");
        assert_eq!(bench.rig().power_w(), Ok(Some(5.0)));

        let bench = Bench::default();
        bench.rig().set_width_hz(2700.0).expect("could not set the filter");
        assert_eq!(bench.heard(), "AI0;BW0270;");

        let bench = Bench::answering("BW0270;");
        assert_eq!(bench.rig().width_hz(), Ok(Some(2700.0)));
    }

    /// An answer is matched to its question, not taken in turn.
    ///
    /// A KX3 volunteers an `FA` of its own each time the frequency moves, and
    /// while the operator is scrolling the dial those arrive among the answers
    /// to everything else. Read in order they put the link one command out of
    /// step: the frequency shown becomes a previous reading, and then a partial
    /// record parses as nothing — "the rig answered nonsense", mid-tune.
    #[test]
    fn records_nobody_asked_for_are_stepped_over() {
        // The radio volunteers two frequency changes, then answers the mode
        // question that was actually asked.
        let bench = Bench::answering("FA00014070000;FA00007070000;MD6;");
        assert_eq!(bench.rig().dial_mode(), Ok(Some("DATA".to_string())));

        // And the frequency question gets the *last* thing the radio said about
        // frequency, not the first, because they arrive in order.
        let bench = Bench::answering("MD2;FA00007070000;");
        assert_eq!(bench.rig().dial_hz(), Ok(Some(7_070_000.0)));
    }

    /// A radio that will not stop talking is a link to give up on rather than
    /// one to read for ever.
    #[test]
    fn a_radio_that_only_volunteers_is_given_up_on() {
        let babble = "MD6;".repeat(20);
        let bench = Bench::answering(&babble);
        assert!(matches!(bench.rig().dial_hz(), Err(Fault::Protocol(_))));
    }

    /// A radio that says nothing is a fault, not an empty answer.
    #[test]
    fn a_silent_radio_times_out() {
        let bench = Bench::default();
        assert_eq!(bench.rig().dial_hz(), Err(Fault::Timeout));
    }

    /// A record for another command is stepped over; one that *is* the answer
    /// and makes no sense is reported.
    ///
    /// Two different faults, and they send an operator to different places: the
    /// first is a radio talking over itself and resolves on the next read, the
    /// second is a radio saying something this cannot parse.
    #[test]
    fn a_wrong_answer_and_an_unreadable_one_are_different_faults() {
        // Only records for other commands: nothing to answer with, so it is a
        // silence rather than nonsense.
        let bench = Bench::answering("MD6;");
        assert_eq!(bench.rig().dial_hz(), Err(Fault::Timeout));

        // The right record, with something unreadable in it.
        let bench = Bench::answering("FAnotafrequency;");
        assert!(matches!(bench.rig().dial_hz(), Err(Fault::Protocol(_))));
    }

}
