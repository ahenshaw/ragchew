//! Channel tracking: associate per-cycle decodes into persistent "channels"
//! (one per station/conversation) by carrier frequency, tolerating drift, so a
//! station's text stays on one row over time.

/// A single decoded frame handed to the tracker.
#[derive(Clone, Debug)]
pub struct Decode {
    pub hz: f64,
    pub time_s: f64,
    pub text: String,
    pub mode: js8::Mode,
}

/// A tracked signal accumulating text across cycles.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: u64,
    /// Latest carrier frequency (tracks drift).
    pub hz: f64,
    /// (time_s, hz) samples, for drawing the drifting streak / leader endpoint.
    pub hz_history: Vec<(f64, f64)>,
    /// Accumulated decoded text, oldest first.
    pub text: String,
    /// Submode of the most recent frame.
    pub mode: js8::Mode,
    /// Character count of the most recently appended frame's text (for the
    /// scroll-in animation).
    pub last_chunk_chars: usize,
    /// Audio time (s) of the most recent frame.
    pub last_heard_s: f64,
    /// Audio time (s) of the first frame.
    pub first_heard_s: f64,
}

/// Tracks channels as decodes arrive (in time order).
#[derive(Clone)]
pub struct ChannelSet {
    channels: Vec<Channel>,
    next_id: u64,
    /// Frequency association tolerance (Hz).
    pub tol_hz: f64,
}

impl ChannelSet {
    pub fn new(tol_hz: f64) -> ChannelSet {
        ChannelSet { channels: Vec::new(), next_id: 0, tol_hz }
    }

    /// Ingest a decode: append to the nearest channel within `tol_hz`, else
    /// start a new channel. Decodes are expected in non-decreasing `time_s`.
    pub fn add(&mut self, d: Decode) {
        // Association tolerance is at least the mode's tone spacing, so a
        // wide-spacing mode (e.g. Turbo, 20 Hz bins) whose per-frame frequency
        // estimate lands in adjacent bins still tracks as one channel.
        let tol = self.tol_hz.max(js8::submode::of(d.mode).spacing());
        let mut best: Option<(usize, f64)> = None;
        for (i, ch) in self.channels.iter().enumerate() {
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

    fn d(hz: f64, t: f64, s: &str) -> Decode {
        Decode { hz, time_s: t, text: s.to_string(), mode: js8::Mode::Normal }
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
}
