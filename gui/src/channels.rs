//! Channel tracking: associate individual decodes into persistent "channels"
//! (one per station/conversation) by carrier frequency, tolerating drift, so a
//! station's text stays on one row over time.
//!
//! A "decode" is one JS8 frame or one Olivia FEC block — the tracker does not
//! care which, only where it landed and when.

use ragchew::protocol::{Decode, ModeId};

/// A tracked signal accumulating text across transmissions.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: u64,
    /// Latest carrier frequency (tracks drift).
    pub hz: f64,
    /// (time_s, hz) samples, for drawing the drifting streak / leader endpoint.
    pub hz_history: Vec<(f64, f64)>,
    /// Accumulated decoded text, oldest first.
    pub text: String,
    /// Mode of the most recent decode.
    pub mode: ModeId,
    /// Character count of the most recently appended chunk (for the scroll-in
    /// animation).
    pub last_chunk_chars: usize,
    /// Audio time (s) of the most recent decode.
    pub last_heard_s: f64,
    /// Audio time (s) of the first decode.
    pub first_heard_s: f64,
}

/// Tracks channels as decodes arrive (in time order).
#[derive(Clone)]
pub struct ChannelSet {
    channels: Vec<Channel>,
    next_id: u64,
    /// Baseline frequency association tolerance (Hz). Modes wider than this ask
    /// for more; see [`ModeId::assoc_tol_hz`].
    pub tol_hz: f64,
}

impl ChannelSet {
    pub fn new(tol_hz: f64) -> ChannelSet {
        ChannelSet { channels: Vec::new(), next_id: 0, tol_hz }
    }

    /// Ingest a decode: append to the nearest channel within tolerance, else
    /// start a new channel. Decodes are expected in non-decreasing `time_s`.
    pub fn add(&mut self, d: Decode) {
        // Tolerance is at least what the mode itself needs: a wide-spacing mode
        // (JS8 Turbo's 20 Hz bins, an Olivia signal 1 kHz across) whose
        // per-decode frequency estimate lands a bin over still tracks as one
        // channel. Decodes only ever join a channel of the same protocol, so a
        // wide Olivia signal cannot swallow a JS8 station sitting inside it.
        let tol = self.tol_hz.max(d.mode.assoc_tol_hz());
        let mut best: Option<(usize, f64)> = None;
        for (i, ch) in self.channels.iter().enumerate() {
            if ch.mode.protocol() != d.mode.protocol() {
                continue;
            }
            let dhz = (ch.hz - d.hz).abs();
            if dhz <= tol && best.map_or(true, |(_, bd)| dhz < bd) {
                best = Some((i, dhz));
            }
        }
        match best {
            Some((i, _)) => {
                let ch = &mut self.channels[i];
                ch.hz = d.hz;
                ch.mode = d.mode;
                ch.hz_history.push((d.time_s, d.hz));
                ch.last_chunk_chars = d.text.chars().count();
                ch.text.push_str(&d.text);
                ch.last_heard_s = d.time_s;
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.channels.push(Channel {
                    id,
                    hz: d.hz,
                    mode: d.mode,
                    hz_history: vec![(d.time_s, d.hz)],
                    last_chunk_chars: d.text.chars().count(),
                    text: d.text,
                    last_heard_s: d.time_s,
                    first_heard_s: d.time_s,
                });
            }
        }
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    /// Build a channel set from a batch of decodes (sorted by time first).
    pub fn from_decodes(decodes: impl IntoIterator<Item = Decode>, tol_hz: f64) -> ChannelSet {
        let mut ds: Vec<Decode> = decodes.into_iter().collect();
        ds.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
        let mut set = ChannelSet::new(tol_hz);
        for d in ds {
            set.add(d);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ragchew::{js8, olivia};

    fn d(hz: f64, t: f64, s: &str) -> Decode {
        Decode {
            hz,
            time_s: t,
            text: s.to_string(),
            mode: ModeId::Js8(js8::Mode::Normal),
            quality: 1.0,
        }
    }

    #[test]
    fn associates_by_frequency_and_accumulates_text() {
        let set = ChannelSet::from_decodes(
            [d(1000.0, 0.0, "A"), d(1005.0, 15.0, "B"), d(1500.0, 15.0, "X")],
            15.0,
        );
        assert_eq!(set.channels().len(), 2);
        let c0 = &set.channels()[0];
        assert_eq!(c0.text, "AB");
        assert_eq!(c0.hz, 1005.0); // tracks to latest
        assert_eq!(set.channels()[1].text, "X");
    }

    #[test]
    fn drift_beyond_tolerance_splits_into_two() {
        // 30 Hz apart with a 15 Hz tolerance -> two channels
        let set = ChannelSet::from_decodes([d(1000.0, 0.0, "A"), d(1030.0, 15.0, "B")], 15.0);
        assert_eq!(set.channels().len(), 2);
    }

    /// A JS8 station transmitting inside a wide Olivia signal's bandwidth is
    /// still its own conversation.
    #[test]
    fn protocols_never_share_a_channel() {
        let ol = Decode {
            hz: 1000.0,
            time_s: 0.0,
            text: "OL".to_string(),
            mode: ModeId::Olivia(olivia::OL_32_1000),
            quality: 4.0,
        };
        let set = ChannelSet::from_decodes([ol, d(1010.0, 1.0, "JS")], 15.0);
        assert_eq!(set.channels().len(), 2);
    }

    /// Successive Olivia blocks of one transmission join up, even though the
    /// mode's frequency estimate is only good to a tone spacing.
    #[test]
    fn olivia_blocks_accumulate_into_one_channel() {
        let block = |hz: f64, t: f64, s: &str| Decode {
            hz,
            time_s: t,
            text: s.to_string(),
            mode: ModeId::Olivia(olivia::OL_16_500),
            quality: 5.0,
        };
        let set = ChannelSet::from_decodes(
            [block(1500.0, 0.0, "CQ C"), block(1508.0, 2.0, "Q DE"), block(1492.0, 4.0, " W1AW")],
            15.0,
        );
        assert_eq!(set.channels().len(), 1);
        assert_eq!(set.channels()[0].text, "CQ CQ DE W1AW");
    }
}
