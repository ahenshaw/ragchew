//! PSK31 varicode: the variable-length character code BPSK31 carries.
//!
//! Two rules define it. No code contains `00`, and every code begins and ends
//! with `1` — so `00` can never occur inside a character and is free to mean
//! "character boundary". Common letters get short codes (`e` is `11`, a space
//! is a single `1`), rare ones get long, and the stream is self-framing without
//! a sync pattern: a receiver that joins mid-transmission is in step by the
//! next separator.
//!
//! That framing is also the only thing standing between a BPSK31 demodulator
//! and nonsense, since the mode carries no FEC at all — a bit pattern that is
//! not in the table is not a character, and [`decode`] drops it.
//!
//! # Scope
//!
//! Printable ASCII (32–126) only. The C0 control block is deliberately absent:
//! this is a keyboard-to-keyboard monitor, control characters carry nothing it
//! would print, and a wrong code there is worse than a missing one. Text handed
//! to [`encode`] that is outside the range is skipped.
//!
//! The table below is checked digit for digit against fldigi's
//! `pskvaricode.cxx` by `tests/psk_reference.rs`, as well as for internal
//! consistency — the two structural rules above, plus uniqueness. The reference
//! check is the one that matters: varicode is 256 hand-assigned codes rather
//! than an algorithm, and since the mode carries no FEC and no CRC, a single
//! transcribed digit would decode as a plausible *wrong* character with nothing
//! downstream to notice. The structural tests cannot see that — they pass
//! happily on a table with a corrupted code in it.

/// (character, code) pairs, in ASCII order. Codes are written MSB first.
pub const VARICODE: &[(char, &str)] = &[
    (' ', "1"),
    ('!', "111111111"),
    ('"', "101011111"),
    ('#', "111110101"),
    ('$', "111011011"),
    ('%', "1011010101"),
    ('&', "1010111011"),
    ('\'', "101111111"),
    ('(', "11111011"),
    (')', "11110111"),
    ('*', "101101111"),
    ('+', "111011111"),
    (',', "1110101"),
    ('-', "110101"),
    ('.', "1010111"),
    ('/', "110101111"),
    ('0', "10110111"),
    ('1', "10111101"),
    ('2', "11101101"),
    ('3', "11111111"),
    ('4', "101110111"),
    ('5', "101011011"),
    ('6', "101101011"),
    ('7', "110101101"),
    ('8', "110101011"),
    ('9', "110110111"),
    (':', "11110101"),
    (';', "110111101"),
    ('<', "111101101"),
    ('=', "1010101"),
    ('>', "111010111"),
    ('?', "1010101111"),
    ('@', "1010111101"),
    ('A', "1111101"),
    ('B', "11101011"),
    ('C', "10101101"),
    ('D', "10110101"),
    ('E', "1110111"),
    ('F', "11011011"),
    ('G', "11111101"),
    ('H', "101010101"),
    ('I', "1111111"),
    ('J', "111111101"),
    ('K', "101111101"),
    ('L', "11010111"),
    ('M', "10111011"),
    ('N', "11011101"),
    ('O', "10101011"),
    ('P', "11010101"),
    ('Q', "111011101"),
    ('R', "10101111"),
    ('S', "1101111"),
    ('T', "1101101"),
    ('U', "101010111"),
    ('V', "110110101"),
    ('W', "101011101"),
    ('X', "101110101"),
    ('Y', "101111011"),
    ('Z', "1010101101"),
    ('[', "111110111"),
    ('\\', "111101111"),
    (']', "111111011"),
    ('^', "1010111111"),
    ('_', "101101101"),
    ('`', "1011011111"),
    ('a', "1011"),
    ('b', "1011111"),
    ('c', "101111"),
    ('d', "101101"),
    ('e', "11"),
    ('f', "111101"),
    ('g', "1011011"),
    ('h', "101011"),
    ('i', "1101"),
    ('j', "111101011"),
    ('k', "10111111"),
    ('l', "11011"),
    ('m', "111011"),
    ('n', "1111"),
    ('o', "111"),
    ('p', "111111"),
    ('q', "110111111"),
    ('r', "10101"),
    ('s', "10111"),
    ('t', "101"),
    ('u', "110111"),
    ('v', "1111011"),
    ('w', "1101011"),
    ('x', "11011111"),
    ('y', "1011101"),
    ('z', "111010101"),
    ('{', "1010110111"),
    ('|', "110111011"),
    ('}', "1010110101"),
    ('~', "1011010111"),
];

/// The code for one character, or `None` if it is outside the table.
pub fn code_of(c: char) -> Option<&'static str> {
    VARICODE.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// The character for one code, or `None` if the bits are not a valid code.
///
/// This is where noise gets thrown out: most bit patterns are not in the table.
pub fn char_of(code: &str) -> Option<char> {
    VARICODE.iter().find(|(_, v)| *v == code).map(|(k, _)| *k)
}

/// Encode text to a bit stream: each character's code followed by the `00`
/// separator. Characters outside the table are skipped.
///
/// No idle is added here — see [`modem::encode`](super::modem::encode), which
/// tops and tails a transmission with it.
pub fn encode(text: &str) -> Vec<u8> {
    let mut bits = Vec::new();
    for c in text.chars() {
        let Some(code) = code_of(c) else { continue };
        bits.extend(code.bytes().map(|b| b - b'0'));
        bits.extend([0, 0]);
    }
    bits
}

/// A character recovered from the bit stream, and the index of the bit that
/// finished it — which is what puts it in time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decoded {
    pub bit: usize,
    pub ch: char,
}

/// Decode a bit stream into characters, splitting on the `00` separator.
///
/// Idle is an unbroken run of zeros, which carries no characters. Anything
/// between separators that is not in the table is dropped rather than guessed
/// at.
///
/// **The stream is assumed to be joined at an arbitrary point**, because that
/// is what a band scan does: a block starts where it starts, quite possibly
/// halfway through a character. The bits before the first separator are
/// therefore discarded — a truncated code can easily *be* a valid shorter code
/// (chop the front off `h` and you have an `e`), and inventing a character is
/// worse than losing one. Callers that know they hold the true start of a
/// transmission should prepend a couple of idle bits.
pub fn decode(bits: &[u8]) -> Vec<Decoded> {
    decode_framed(bits).0
}

/// How a bit stream frames, beyond just what characters fell out of it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Framing {
    /// Separator-delimited groups that held bits — idle is not an attempt.
    pub words: usize,
    /// Of those, how many were in the table.
    pub valid: usize,
    /// Runs of two or more zeros, excluding those long enough to be idle.
    pub gaps: usize,
    /// Of those, how many were exactly two zeros long.
    pub tight_gaps: usize,
    /// Total bits in the words that were attempted.
    pub word_bits: usize,
}

impl Framing {
    /// Fraction of attempted words that were real characters.
    pub fn valid_ratio(&self) -> f64 {
        ratio(self.valid, self.words)
    }
    /// Fraction of gaps that were exactly the two-zero separator.
    ///
    /// This is the sharp one. Varicode codes begin and end with `1`, so during
    /// text every gap is exactly `00` — three zeros in a row cannot happen
    /// unless the station is idling. A stream of arbitrary bits has geometric
    /// gaps and only half of them are two long.
    pub fn tight_ratio(&self) -> f64 {
        ratio(self.tight_gaps, self.gaps)
    }
    /// Mean bits per attempted word. Real varicode averages six or seven;
    /// arbitrary bits stumble into far shorter words.
    pub fn mean_word_bits(&self) -> f64 {
        ratio(self.word_bits, self.words)
    }
}

fn ratio(a: usize, b: usize) -> f64 {
    if b > 0 {
        a as f64 / b as f64
    } else {
        0.0
    }
}

/// Zero-run length at or beyond which a gap is idle rather than a separator,
/// and so says nothing about how the text is framed.
const IDLE_RUN: usize = 4;

/// [`decode`], plus how many words it *attempted* — separator-delimited groups
/// that actually held bits.
///
/// The distinction is the whole point. Idle is an unbroken run of zeros, so a
/// naive count of `00` pairs scores a second of it as thirty-odd words that
/// produced no characters — and idle is what tops and tails every transmission
/// and what an operator sends while thinking. Counting it as failed words
/// buries a real signal's framing ratio under its own silence.
pub fn decode_framed(bits: &[u8]) -> (Vec<Decoded>, Framing) {
    let mut s = Stream::new();
    let out = bits.iter().filter_map(|&b| s.push(b)).collect();
    (out, s.framing)
}

/// The same framing, one bit at a time.
///
/// A block decoder has the whole bit stream in front of it; a receiver locked
/// to a live carrier has one bit and whatever it remembers. Both need to frame
/// varicode by exactly the same rules — where a character ends, what counts as
/// idle rather than a failed word, and that the bits before the first separator
/// are thrown away — so [`decode_framed`] is written in terms of this rather
/// than beside it. Two implementations of a rule this fiddly would not stay
/// the same rule for long.
#[derive(Clone, Debug, Default)]
pub struct Stream {
    cur: String,
    run: usize,
    prev_zero: bool,
    synced: bool,
    n: usize,
    /// How what has gone past frames, for the gate to weigh.
    pub framing: Framing,
}

impl Stream {
    pub fn new() -> Stream {
        Stream::default()
    }

    /// Feed one bit; yields a character when one completes.
    ///
    /// The index in the returned [`Decoded`] counts bits since this stream
    /// began, so a caller that knows when its first bit was on the air knows
    /// when the character was.
    pub fn push(&mut self, bit: u8) -> Option<Decoded> {
        let i = self.n;
        self.n += 1;
        if bit != 0 {
            if (2..IDLE_RUN).contains(&self.run) && self.synced {
                self.framing.gaps += 1;
                if self.run == 2 {
                    self.framing.tight_gaps += 1;
                }
            }
            self.run = 0;
            self.cur.push('1');
            self.prev_zero = false;
            return None;
        }
        self.run += 1;
        if !self.prev_zero {
            self.prev_zero = true;
            // Leading zeros are idle, not part of any character.
            if !self.cur.is_empty() {
                self.cur.push('0');
            }
            return None;
        }
        // The second zero of a run: a separator, whatever came before it.
        let mut got = None;
        if !self.cur.is_empty() {
            // `cur` ends with the separator's first zero.
            if self.synced {
                let code = &self.cur[..self.cur.len() - 1];
                self.framing.words += 1;
                self.framing.word_bits += code.len();
                if let Some(ch) = char_of(code) {
                    self.framing.valid += 1;
                    got = Some(Decoded { bit: i, ch });
                }
            }
            self.cur.clear();
        }
        self.synced = true;
        got
    }

    /// Bits pushed so far.
    pub fn bits(&self) -> usize {
        self.n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two rules that make the code self-framing. If either fails, `00`
    /// stops meaning "character boundary" and nothing decodes.
    #[test]
    fn every_code_is_well_formed() {
        for (c, code) in VARICODE {
            assert!(!code.is_empty(), "{c:?} has no code");
            assert!(code.starts_with('1'), "{c:?} = {code} does not start with 1");
            assert!(code.ends_with('1'), "{c:?} = {code} does not end with 1");
            assert!(!code.contains("00"), "{c:?} = {code} contains the separator");
            assert!(code.bytes().all(|b| b == b'0' || b == b'1'), "{c:?} = {code} is not binary");
        }
    }

    #[test]
    fn codes_and_characters_are_unique() {
        let mut codes: Vec<&str> = VARICODE.iter().map(|(_, v)| *v).collect();
        codes.sort_unstable();
        let n = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), n, "duplicate code in the table");

        let mut chars: Vec<char> = VARICODE.iter().map(|(k, _)| *k).collect();
        chars.sort_unstable();
        let n = chars.len();
        chars.dedup();
        assert_eq!(chars.len(), n, "duplicate character in the table");
    }

    #[test]
    fn covers_printable_ascii() {
        for c in ' '..='~' {
            assert!(code_of(c).is_some(), "{c:?} is missing from the table");
        }
        assert_eq!(VARICODE.len(), 95);
    }

    /// The short codes are the ones that decide the mode's throughput, and they
    /// are the ones a transcription error would be least likely to survive.
    #[test]
    fn the_common_characters_are_where_they_should_be() {
        assert_eq!(code_of(' '), Some("1"));
        assert_eq!(code_of('e'), Some("11"));
        assert_eq!(code_of('t'), Some("101"));
        assert_eq!(code_of('a'), Some("1011"));
        assert_eq!(code_of('o'), Some("111"));
        assert_eq!(code_of('i'), Some("1101"));
        assert_eq!(code_of('n'), Some("1111"));
    }

    /// Idle in front, because `decode` joins mid-stream by default and drops
    /// whatever precedes the first separator.
    fn framed(text: &str) -> Vec<u8> {
        let mut bits = vec![0u8, 0];
        bits.extend(encode(text));
        bits
    }

    #[test]
    fn round_trips() {
        let text = "cq cq de w1aw/4 k 73!";
        let got: String = decode(&framed(text)).iter().map(|d| d.ch).collect();
        assert_eq!(got, text);
    }

    /// Real text frames perfectly, and — the case that matters — so does a
    /// station that idles. Idle is an unbroken run of zeros; counting it as
    /// failed words or loose gaps would reject the most common thing a PSK31
    /// operator does between overs.
    #[test]
    fn idle_does_not_spoil_the_framing_it_surrounds() {
        let text = "cq cq de kb9psk k ";

        let (_, clean) = decode_framed(&framed(text));
        assert_eq!(clean.valid_ratio(), 1.0);
        assert_eq!(clean.tight_ratio(), 1.0);

        // The same text with a second of idle either side of it, which is what
        // goes on the air.
        let mut bits = vec![0u8; 32];
        bits.extend(encode(text));
        bits.extend(vec![0u8; 32]);
        let (chars, idled) = decode_framed(&bits);
        assert_eq!(chars.iter().map(|d| d.ch).collect::<String>(), text);
        assert_eq!(idled.valid_ratio(), 1.0, "idle counted against the words");
        assert_eq!(idled.tight_ratio(), 1.0, "idle counted as a loose gap");
        assert_eq!(idled.words, clean.words, "idle invented words");
    }

    /// A receiver that sees the bits one at a time frames them exactly as a
    /// block decoder with all of them in front of it does — same characters,
    /// same bit indices, same framing counters.
    ///
    /// [`decode_framed`] is written in terms of [`Stream`], so this cannot fail
    /// by the two drifting apart. What it does guard is the refactor that
    /// merged them: it pins the equivalence against inputs — idle, a joined
    /// stream, three-zero runs, an empty one — whose handling is exactly what a
    /// hand-rolled second implementation would have got subtly wrong.
    #[test]
    fn one_bit_at_a_time_frames_the_same_as_a_whole_block() {
        let mut seed = 0x1234_5678u64;
        let mut noise_bits = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    ((seed >> 60) & 1) as u8
                })
                .collect()
        };

        let text = encode("cq de w1aw, 599 k");
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0; 12],                                    // pure idle
            vec![1; 12],                                    // a steady carrier
            text.clone(),                                   // clean text
            [vec![0; 8], text.clone(), vec![0; 8]].concat(), // idle either side
            text[3..].to_vec(),                             // joined mid-character
            [text.clone(), vec![0, 0, 0], text.clone()].concat(), // a three-zero run
            noise_bits(500),                                // nonsense
        ];

        for (i, bits) in cases.iter().enumerate() {
            let (want, want_framing) = decode_framed(bits);
            let mut s = Stream::new();
            let got: Vec<Decoded> = bits.iter().filter_map(|&b| s.push(b)).collect();
            assert_eq!(got, want, "case {i}: characters differ");
            assert_eq!(s.bits(), bits.len(), "case {i}: bit count");
            let (g, w) = (s.framing, want_framing);
            assert_eq!(
                (g.words, g.valid, g.gaps, g.tight_gaps, g.word_bits),
                (w.words, w.valid, w.gaps, w.tight_gaps, w.word_bits),
                "case {i}: framing differs"
            );
        }
    }

    /// A receiver joins mid-transmission and after idle: both must resynchronise
    /// at the next separator without losing anything but the partial character.
    #[test]
    fn resynchronises_after_idle_and_mid_character() {
        let mut bits = vec![0u8; 17]; // idle, an odd number of symbols
        bits.extend(encode("hello"));
        let got: String = decode(&bits).iter().map(|d| d.ch).collect();
        assert_eq!(got, "hello");

        // Start halfway through the 'h'. Its tail, "011", would decode as an
        // 'e' if it were trusted; dropping the first word is what stops that.
        let full = encode("hello");
        let got: String = decode(&full[3..]).iter().map(|d| d.ch).collect();
        assert_eq!(got, "ello");
    }

    /// Characters are placed in time by the bit that completed them, so the
    /// separator's second zero is the reported index.
    #[test]
    fn reports_where_each_character_finished() {
        // Two bits of idle, then "e" = 11, then 00: it completes on bit 5.
        assert_eq!(decode(&framed("e")), vec![Decoded { bit: 5, ch: 'e' }]);
    }

    /// Validity alone is nowhere near enough to keep noise off the screen, and
    /// this is the measurement that says so: **five in six** random words are a
    /// valid character. Varicode covers 95 of the 143 possible words of ten
    /// bits or fewer, and the short ones — which is what arbitrary bits mostly
    /// produce — are all in the table.
    ///
    /// Which is why [`modem`](super::modem) gates on the *signal* before it
    /// demodulates, and why the framing test it applies afterwards leans on
    /// [`Framing::tight_ratio`] rather than on this. This test guards that
    /// assumption: if the figure ever moves far, the table has changed and the
    /// gate's thresholds want re-measuring with it.
    #[test]
    fn validity_alone_is_far_too_leaky_to_rely_on() {
        let mut s = 0x1234_5678_9abc_def0u64;
        let bits: Vec<u8> = (0..200_000)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((s >> 63) & 1) as u8
            })
            .collect();
        let (_, f) = decode_framed(&bits);
        assert!(
            (0.75..0.90).contains(&f.valid_ratio()),
            "random words were {:.3} valid; modem::VALID_RATIO_MIN assumes ~0.83",
            f.valid_ratio()
        );

        // The gap structure is the part that does separate. Arbitrary bits have
        // geometric zero-runs, and gaps at or past `IDLE_RUN` are not counted,
        // so the baseline is P(2)/(P(2)+P(3)) = (1/4)/(1/4+1/8) = 2/3. Real
        // text can only ever produce a gap of two.
        assert!(
            (0.60..0.73).contains(&f.tight_ratio()),
            "random gaps were {:.3} tight; modem::TIGHT_RATIO_MIN assumes ~0.667",
            f.tight_ratio()
        );
    }
}
