//! A Yaesu radio on a cable, with no daemon in between.
//!
//! The current Yaesus — FT-991A, FT-891, FT-710, FTDX10, FTDX101, and back
//! through the FT-450D and FTDX3000 — speak a semicolon-terminated ASCII CAT
//! that is the same *shape* as Elecraft's and agrees with it on almost nothing
//! else. The link is [`super::cat`]; what is here is the vocabulary.
//!
//! | | |
//! |---|---|
//! | `TX1;` / `TX0;` | key and unkey |
//! | `TX;` | whether it is transmitting, and by what |
//! | `FA;` | VFO A, nine digits of hertz |
//! | `FA014074000;` | tune VFO A |
//! | `MD0;` | mode, as a hex digit |
//! | `PC;` | power, three digits of watts |
//! | `AI0;` | stop volunteering: this is a question-and-answer link |
//!
//! Three places where this is *not* the Elecraft driver with different letters:
//!
//! Frequency is nine digits, not eleven. A Yaesu given eleven ignores the
//! command in silence, which reads here as a tune that worked and did nothing.
//!
//! Keying is asked directly. `TX;` answers `TX0;` receiving, `TX1;`
//! transmitting because CAT asked, `TX2;` transmitting because something else
//! did — a front panel MOX, a foot switch. The Elecraft driver has to count
//! columns in an `IF` record for this; here it is a question with an answer, so
//! there is no column to get wrong. The app only needs transmitting-or-not, so
//! `1` and `2` both mean yes, but the distinction is real and is why this reads
//! a set rather than a single digit.
//!
//! **The filter width is not reported.** Yaesu's `SH` is an index into a table
//! of widths that differs by mode and by model, not a number of hertz, so there
//! is no honest way to answer [`Transmitter::width_hz`] from it. It is left
//! unimplemented, which the app already handles: the filter control does not
//! appear and the waterfall is not shaded. That is the correct outcome — the
//! shading is drawn straight from this number, and a made-up table would shade
//! the wrong part of the band while looking authoritative.

use std::io::{Read, Write};

use super::cat::Cat;
use super::{
    Fault, Transmitter, KEYING, READING_DIAL, READING_PTT, SETTING_MODE, SETTING_POWER, TUNING,
};

/// The modes a Yaesu will answer to, in the words this app shows.
///
/// The data modes lead, being what this app is for: on a Yaesu, FT8 and JS8 are
/// worked in `DATA-USB`. The names are the radio's own, which is what makes
/// them worth showing — a station in `DATA-USB` is in the mode its front panel
/// says it is in.
pub const MODES: &[&str] = &[
    "DATA-USB",
    "DATA-LSB",
    "USB",
    "LSB",
    "CW-U",
    "CW-L",
    "RTTY-USB",
    "RTTY-LSB",
    "AM",
    "FM",
];

/// What an FT-991A is set to when it leaves Yaesu.
///
/// Lower than any Elecraft, and worth knowing because the rate is a shared
/// setting: an operator coming from a KX3 at 38400 has to visit menu 031 or
/// change it here. Not the default the app starts at — that stays at the
/// Elecraft's rate, since changing it would be wrong for the other radio.
pub const FT991A_BAUD: u32 = 4800;

/// A Yaesu radio, over anything that carries bytes.
pub struct Yaesu<L> {
    cat: Cat<L>,
    what: String,
}

impl<L: Read + Write + Send> Yaesu<L> {
    pub fn new(link: L, what: String) -> Yaesu<L> {
        Yaesu { cat: Cat::new(link), what }
    }
}

/// The hertz an `FA` answer carries.
///
/// Nine digits on every radio listed above. Read as "all digits" rather than
/// "exactly nine", because a model that spends ten on it is still telling the
/// truth in the same units, and the frequency is checked for plausibility by
/// the caller either way.
fn frequency_in(answer: &str) -> Option<f64> {
    let digits = answer.strip_prefix("FA")?;
    if digits.len() < 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<f64>().ok()
}

/// What the radio calls the mode it is in.
///
/// The FT-991A's table, which the FT-891, FT-710 and FTDX10 share for
/// everything below `E`. `C4FM` is Yaesu's own digital voice and is on the
/// radios that have it; the others answer the same digits.
fn mode_name(digit: char) -> &'static str {
    match digit {
        '1' => "LSB",
        '2' => "USB",
        '3' => "CW-U",
        '4' => "FM",
        '5' => "AM",
        '6' => "RTTY-LSB",
        '7' => "CW-L",
        '8' => "DATA-LSB",
        '9' => "RTTY-USB",
        'A' | 'a' => "DATA-FM",
        'B' | 'b' => "FM-N",
        'C' | 'c' => "DATA-USB",
        'D' | 'd' => "AM-N",
        'E' | 'e' => "C4FM",
        _ => "?",
    }
}

/// The digit a Yaesu wants for a mode this app names.
///
/// Only the modes in [`MODES`] are here. `DATA-FM`, `FM-N`, `AM-N` and `C4FM`
/// can be read back and named but are not offered to be set: they are not modes
/// this app has anything to say in, and a menu is more useful short.
pub(super) fn mode_digit(name: &str) -> Option<char> {
    Some(match name {
        "LSB" => '1',
        "USB" => '2',
        "CW-U" => '3',
        "FM" => '4',
        "AM" => '5',
        "RTTY-LSB" => '6',
        "CW-L" => '7',
        "DATA-LSB" => '8',
        "RTTY-USB" => '9',
        "DATA-USB" => 'C',
        _ => return None,
    })
}

/// Whether a `TX` answer says the radio is transmitting.
///
/// `0` receiving, `1` transmitting under CAT, `2` transmitting because
/// something else asked — and the last of those is exactly the case this app
/// draws differently, a hand on the front panel rather than its own key-down.
/// Both count as transmitting here; which of them it was is the app's own
/// comparison against what it asked for.
///
/// Anything else gives `None` rather than a guess. The whole reason for asking
/// is to catch a rig keyed by something other than this app, and an indicator
/// that invented an answer would invent exactly that.
fn transmitting_in(body: &str) -> Option<bool> {
    match body.trim() {
        "0" => Some(false),
        "1" | "2" => Some(true),
        _ => None,
    }
}

impl<L: Read + Write + Send> Transmitter for Yaesu<L> {
    fn key(&mut self, on: bool) -> Result<(), Fault> {
        // Neither is answered, so neither is read back. The rig is asked what
        // it is doing separately, which is the only honest way to know.
        self.cat.tell(if on { "TX1;" } else { "TX0;" }).map_err(|e| match e {
            Fault::Link(m) => Fault::Link(format!("{KEYING}: {m}")),
            other => other,
        })
    }

    fn keyed(&mut self) -> Result<Option<bool>, Fault> {
        let answer = self.cat.ask("TX;")?;
        let Some(body) = answer.strip_prefix("TX") else {
            return Err(Fault::Protocol(format!(
                "{READING_PTT}: expected a TX record, got {answer:?}"
            )));
        };
        Ok(transmitting_in(body))
    }

    fn dial_hz(&mut self) -> Result<Option<f64>, Fault> {
        let answer = self.cat.ask("FA;")?;
        match frequency_in(&answer) {
            Some(hz) => Ok(Some(hz)),
            None => Err(Fault::Protocol(format!(
                "{READING_DIAL}: expected an FA record, got {answer:?}"
            ))),
        }
    }

    fn tune(&mut self, hz: f64) -> Result<(), Fault> {
        let hz = hz.max(0.0).round() as u64;
        // Nine digits and no more. A longer command is one the radio ignores in
        // silence, which would read here as a tune that worked.
        if hz > 999_999_999 {
            return Err(Fault::Rejected { doing: TUNING, code: -1 });
        }
        self.cat.tell(&format!("FA{hz:09};"))
    }

    fn dial_mode(&mut self) -> Result<Option<String>, Fault> {
        let answer = self.cat.ask("MD0;")?;
        // `MD0C;` — the `0` selects the main receiver and what follows is the
        // mode. The whole `MD0` is stripped rather than just `MD`, and that is
        // not belt and braces: `ask` has already matched the answer against
        // the `MD0` it sent, so anything reaching here carries it.
        match answer.strip_prefix("MD0").and_then(|d| d.chars().next()) {
            Some(digit) => Ok(Some(mode_name(digit).to_string())),
            None => Err(Fault::Protocol(format!("expected an MD record, got {answer:?}"))),
        }
    }

    fn set_mode(&mut self, mode: &str) -> Result<(), Fault> {
        // A word this radio has no digit for is refused rather than sent: a
        // mode command with nonsense in it is ignored silently, which would
        // read here as a change that worked and did nothing.
        let Some(digit) = mode_digit(mode) else {
            return Err(Fault::Rejected { doing: SETTING_MODE, code: -1 });
        };
        self.cat.tell(&format!("MD0{digit};"))
    }

    fn power_w(&mut self) -> Result<Option<f64>, Fault> {
        // `PCnnn;` — watts, three digits, same as an Elecraft. A radio with a
        // hundred-watt PA answers `PC100;`.
        let answer = self.cat.ask("PC;")?;
        Ok(answer.strip_prefix("PC").and_then(|d| d.trim().parse::<f64>().ok()))
    }

    fn set_power_w(&mut self, watts: f64) -> Result<(), Fault> {
        let w = watts.round().clamp(0.0, 999.0) as u32;
        let _ = SETTING_POWER;
        self.cat.tell(&format!("PC{w:03};"))
    }

    // `width_hz` and `set_width_hz` are deliberately left at the trait's
    // defaults — see the note at the top of this file. `SH` is an index into a
    // per-mode table, not hertz, and a guess here would shade the waterfall
    // wrongly while looking certain.
    //
    // Neither range is given either. `PC` tops out at 100 W on an FT-991A and
    // an FT-891, and at 200 on an FTDX101 — a per-model number, and `ID;` does
    // return a model code that would settle it. Writing that table from a
    // manual, with no radio to check a single entry against, is how a slider
    // ends up stopping at the wrong watt on somebody's radio. The value box
    // stands until one of these is on a bench.

    fn describe(&self) -> String {
        format!("Yaesu on {}", self.what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rig::cat::bench::Bench;

    /// The bench radio, wired to this driver.
    trait AsYaesu {
        fn rig(&self) -> Yaesu<Bench>;
    }

    impl AsYaesu for Bench {
        fn rig(&self) -> Yaesu<Bench> {
            Yaesu::new(self.clone(), "a bench".to_string())
        }
    }

    /// Keying is two commands and no answer, and the radio is told to stop
    /// volunteering before anything else is said to it.
    ///
    /// `TX1;`/`TX0;`, not the Elecraft's `TX;`/`RX;` — a Yaesu reads a bare
    /// `TX;` as the *question*, so keying with it would ask whether the radio
    /// is transmitting and never key at all.
    #[test]
    fn keying_says_tx1_and_tx0_and_waits_for_nothing() {
        let bench = Bench::default();
        let mut rig = bench.rig();
        rig.key(true).expect("could not key");
        rig.key(false).expect("could not unkey");
        assert_eq!(bench.heard(), "AI0;TX1;TX0;");
    }

    /// Nine digits of hertz, not the Elecraft's eleven.
    ///
    /// The one that would be silently wrong: a Yaesu given an eleven-digit
    /// `FA` ignores it, so the app would show a frequency it had asked for and
    /// the radio would sit where it was.
    #[test]
    fn the_frequency_is_nine_digits() {
        let bench = Bench::answering("FA014074000;");
        assert_eq!(bench.rig().dial_hz(), Ok(Some(14_074_000.0)));

        let bench = Bench::default();
        bench.rig().tune(7_078_000.0).expect("could not tune");
        assert_eq!(bench.heard(), "AI0;FA007078000;");

        // Exactly nine, so the command is twelve characters with its `;`.
        let sent = bench.heard();
        let fa = sent.strip_prefix("AI0;").unwrap();
        assert_eq!(fa.len(), 12, "an FA command of the wrong width: {fa:?}");

        // Above what nine digits can carry is refused rather than truncated.
        let bench = Bench::default();
        assert!(matches!(
            bench.rig().tune(1_000_000_000.0),
            Err(Fault::Rejected { doing: TUNING, .. })
        ));
        assert_eq!(bench.heard(), "", "sent a frequency the radio cannot take");
    }

    /// Transmitting is asked directly, and a rig keyed by hand still reads as
    /// transmitting.
    ///
    /// `TX2` is the case the app draws amber — the rig is on the air for a
    /// reason this app did not ask for. Reading it as "not transmitting" would
    /// be the exact failure the read-back exists to prevent.
    #[test]
    fn a_rig_keyed_by_hand_still_reads_as_transmitting() {
        let bench = Bench::answering("TX0;");
        assert_eq!(bench.rig().keyed(), Ok(Some(false)));

        let bench = Bench::answering("TX1;");
        assert_eq!(bench.rig().keyed(), Ok(Some(true)), "read a keyed radio as receiving");

        let bench = Bench::answering("TX2;");
        assert_eq!(
            bench.rig().keyed(),
            Ok(Some(true)),
            "a rig keyed from its front panel read as receiving"
        );

        // An answer of another shape is not guessed at.
        let bench = Bench::answering("TX9;");
        assert_eq!(bench.rig().keyed(), Ok(None));
    }

    /// The mode comes back as the word on the radio's own front panel.
    #[test]
    fn the_mode_comes_back_as_a_word() {
        for (digit, name) in
            [('C', "DATA-USB"), ('8', "DATA-LSB"), ('2', "USB"), ('1', "LSB"), ('3', "CW-U")]
        {
            let bench = Bench::answering(&format!("MD0{digit};"));
            assert_eq!(bench.rig().dial_mode(), Ok(Some(name.to_string())), "MD0{digit}");
        }
        // The question is `MD0;`, so only an `MD0…` record answers it. A bare
        // `MDC;` is a record for something else and is stepped over — which
        // leaves the question unanswered rather than answered wrongly, and is
        // the whole point of matching answers to questions.
        let bench = Bench::answering("MDC;");
        assert_eq!(bench.rig().dial_mode(), Err(Fault::Timeout));
    }

    /// Setting a mode carries the receiver digit, and a word the radio has no
    /// digit for is refused rather than sent.
    #[test]
    fn the_mode_goes_out_with_the_receiver_digit() {
        let bench = Bench::default();
        bench.rig().set_mode("DATA-USB").expect("could not set the mode");
        assert_eq!(bench.heard(), "AI0;MD0C;");

        let bench = Bench::default();
        assert!(matches!(
            bench.rig().set_mode("DATA"),
            Err(Fault::Rejected { doing: SETTING_MODE, .. })
        ));
        // Nothing at all: an Elecraft's word for it is refused before the port
        // is even greeted, rather than sent for the radio to ignore.
        assert_eq!(bench.heard(), "", "sent a mode the radio has no digit for");
    }

    /// Every mode offered is one the radio has a digit for, and every digit the
    /// radio can answer with has a name.
    ///
    /// The pair matters: a name in [`MODES`] with no digit would be a menu
    /// entry that silently does nothing, and a digit with no name would show
    /// the operator a `?` for a mode their radio is plainly in.
    #[test]
    fn the_modes_offered_and_the_modes_understood_agree() {
        for name in MODES {
            let digit = mode_digit(name).unwrap_or_else(|| panic!("{name} is offered with no digit"));
            assert_eq!(mode_name(digit), *name, "{name} does not survive the round trip");
        }
        for digit in "123456789ABCDE".chars() {
            assert_ne!(mode_name(digit), "?", "the radio can answer {digit} and it has no name");
        }
    }

    /// Power in watts, three digits, which is the one unit this shares with an
    /// Elecraft.
    #[test]
    fn power_goes_out_in_watts() {
        let bench = Bench::default();
        bench.rig().set_power_w(50.0).expect("could not set power");
        assert_eq!(bench.heard(), "AI0;PC050;");

        let bench = Bench::answering("PC100;");
        assert_eq!(bench.rig().power_w(), Ok(Some(100.0)));
    }

    /// The filter is not answered rather than answered wrongly.
    ///
    /// Yaesu's `SH` is an index into a per-mode table, so there is no width in
    /// hertz to give. `None` is what the app draws as "cannot be asked": no
    /// filter control, and no shading on the waterfall. A number invented here
    /// would shade the wrong part of the band and look certain doing it.
    #[test]
    fn the_filter_width_is_not_guessed_at() {
        let bench = Bench::answering("SH021;");
        assert_eq!(bench.rig().width_hz(), Ok(None), "invented a filter width");
        // And nothing was said to the radio to find that out.
        assert_eq!(bench.heard(), "", "asked the radio about a width it cannot report");
    }

    /// Neither setting claims a range, and that is deliberate.
    ///
    /// `PC` tops out at 100 W on an FT-991A and 200 on an FTDX101 — a
    /// per-model number that `ID;` would settle, and that writing from a manual
    /// with no radio to check would settle wrongly. The width has no range for
    /// the simpler reason that it has no value either.
    ///
    /// Here so that filling either in is a deliberate act with a test to
    /// update, rather than something that looks like an oversight.
    #[test]
    fn no_range_is_claimed_for_either_setting() {
        let bench = Bench::default();
        assert_eq!(bench.rig().power_range_w(), Ok(None), "claimed to know the radio's ceiling");
        assert_eq!(bench.rig().width_range_hz(), Ok(None));
        assert_eq!(bench.heard(), "", "asked the radio about limits it was never going to give");
    }

    /// The link's own behaviour, through this driver: a radio that volunteers
    /// records does not put the answers out of step.
    ///
    /// The same guarantee [`crate::rig::cat`] gives the Elecraft, checked here
    /// too because it is the driver's vocabulary that decides which prefix an
    /// answer has to carry — `MD0;` has to be matched by `MD0…`, not by any
    /// `MD` the radio happened to say first.
    #[test]
    fn records_nobody_asked_for_are_stepped_over() {
        let bench = Bench::answering("FA014070000;FA007070000;MD0C;");
        assert_eq!(bench.rig().dial_mode(), Ok(Some("DATA-USB".to_string())));
    }

    /// A radio that says nothing is a fault, not an empty answer.
    #[test]
    fn a_silent_radio_times_out() {
        let bench = Bench::default();
        assert_eq!(bench.rig().dial_hz(), Err(Fault::Timeout));
    }
}
