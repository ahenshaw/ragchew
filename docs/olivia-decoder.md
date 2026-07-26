# How the Olivia decoder works

Olivia does not reward the usual mental model of a digital-mode receiver —
filter down to the signal, lock to it, demodulate. This is a walk through what
`ragchew` does instead, and why.

Source: [`src/olivia/`](../src/olivia/) — `mode.rs` (parameters), `fht.rs`
(Hadamard transform), `fec.rs` (spreading and block decode), `modem.rs`
(modulator, tone grid, band search).

## The input

The whole ~3 kHz SSB passband at once, resampled to 12 kHz mono, scanning
300–2600 Hz by default. No tuning, no per-signal filtering, no "select a signal
first". Everything below follows from that.

## Why the signal cannot be found the obvious way

Two properties rule out the normal approaches.

**Olivia has no sync pattern.** No preamble, no training sequence, nothing to
correlate against. (JS8, by contrast, hands you three 7-tone Costas arrays; find
those and you have found the signal, its frequency and its timing in one step.)

**The signal can be below the noise floor.** Each character is spread over 64
chips, so a station can put less power into the band than the noise does. In the
`--weak` demo, stations 12 dB under the noise leave no visible trace on the
waterfall and decode perfectly.

So there is nothing to look for and no energy bump to find. **The error-
correcting code is the detector.** We do not find a signal and then decode it;
we attempt to decode everywhere, and the places that decode are where the
signals are.

## The tone grid

One structure per mode does most of the work. Taking Olivia 8/500 — 8 tones
across 500 Hz — as the worked example:

| quantity | value | where it comes from |
|---|---|---|
| tone spacing | 62.5 Hz | bandwidth / tones |
| symbol rate | 62.5 baud | numerically the same: one symbol per tone-spacing period |
| symbol stride | 192 samples | 12000 / 62.5 |
| analysis window | 384 samples | symbols overlap 50%, raised-cosine shaped |
| FFT bin | 31.25 Hz | 12000 / 384 — **exactly half a tone spacing** |

That last row is the crux. Tones land **exactly two bins apart**, and a Hann
window's spectral nulls fall on every other tone's bin, so each tone contributes
zero leakage to its neighbours. That is the orthogonality the whole scheme rests
on, and it is why the analysis window is the symbol *shape* rather than some
independently chosen length.

It also buys a large efficiency. A signal whose lowest tone sits in bin `b` has
its tones at `b, b+2, b+4, …`, so **testing a different centre frequency is just
a different starting index into the same FFT** — not a new transform. One grid
per mode covers every candidate frequency in the band at once.

A second grid, mixed down by half a bin, is computed alongside it. Together they
put any real centre frequency within an eighth of a tone spacing of a grid
point.

## The search

For each hypothesis — **every candidate centre frequency × every block timing**
(a block is 64 symbols, and timing is searched to half a symbol, so 128 phases)
— attempt a Walsh decode:

1. **Soft bits.** Per symbol, per bit position: sum ±(tone energy) across all
   tones, signed by whether that bit is set in the tone's symbol value after
   un-Gray-coding, normalised by the symbol's total energy. Positive means the
   bit is more likely a 0.

2. **De-interleave.** Chip `t` of character `f` rode in bit
   `(f + t) mod bits_per_symbol` of symbol `t`. Each character is smeared across
   every bit position, so a lost tone damages all the characters in a block a
   little rather than one of them fatally.

3. **Un-scramble.** XOR against a fixed 64-bit code
   (`0xE257E6D0291574EC`), rotated 13 bits per character so that the characters
   of one block do not share a pattern.

4. **Correlate.** A 64-point fast Hadamard transform. The largest magnitude of
   the 64 outputs *is* the character: 6 bits from its position, the 7th from its
   sign. That is the entire FEC — no parity checks, just "which of 64 orthogonal
   Walsh functions does this most resemble".

## Deciding whether it is real

The statistic is the correlation peak divided by the root-mean-square of the
other 63.

**Its noise floor is about 2.6, not 0.** Take the largest of 64 correlations
against pure noise and it still stands 2.6 times above the rest. Setting
thresholds below that produces confident garbage across the entire band, which
is exactly what the first version of this decoder did.

Thresholds therefore sit above 2.6, and scale as
`6 / sqrt(4 × chars per block)`. They have to scale: a block's score is the mean
over its characters, so a 4-tone mode averaging 2 correlations has far more
spread around that floor than a 32-tone mode averaging 5 — and a band scan takes
the maximum over millions of hypotheses, which finds every tail.

Two further gates:

- **Sync must be sustained** — the mean of the best run of 4 consecutive blocks,
  not a single lucky one. Scoring the best *run* rather than the average also
  means a station transmitting for ten seconds of a two-minute recording still
  syncs.
- **An isolated block is rejected.** A strong block with quiet neighbours is
  noise getting lucky; a real transmission is blocks on end.

Calibration target: nothing invented from 45 seconds of noise across the whole
band, in any mode, while still copying at roughly −12 dB SNR in a 2.5 kHz
reference bandwidth.

## Making it affordable

The naive search is millions of hypotheses. Two things carry it:

- The tone grid is computed **once per mode** (twice, counting the half-bin
  offset) and reused for every candidate frequency.
- Hypotheses are screened on the **first character only** —
  `1/bits_per_symbol` of the work — and only survivors pay for a full block.

For a minute of audio in one mode that is on the order of a million
single-character correlations: **1–3 seconds**, a quiet band being at the fast
end and a busy one at the slow end, since every plausible timing has to be
decoded properly to be ruled out.

## Sensitivity, and what sets it

Sensitivity tracks **characters per second**, not bandwidth. Every mode spreads
a character over the same 64 chips, so a slower mode spends more energy on each
and hears further:

| mode | chars/s | full copy down to |
|---|---:|---:|
| 4/125 | 0.98 | −12 dB |
| 8/250 | 1.46 | −12 dB |
| 16/500 | 1.95 | −12 dB |
| 32/1000 | 2.44 | −12 dB |
| 8/500 | 2.93 | −9 dB |
| 16/1000 | 3.91 | −9 dB |

(SNR in a 2.5 kHz reference bandwidth, measured against white noise.) The
decline is not gentle: the window between clean copy and nothing at all is about
a decibel wide.

## What real signals forced

Two design decisions came from testing against an off-air recording rather than
from synthetic audio, and both were bugs that synthetic tests could not see.

**Every timing that syncs is kept, not just the best one per frequency.** Two
stations working each other sit a fraction of a tone spacing apart — 968.8 and
984.4 Hz in the recording that found this — and take turns, each keying up on
its own block boundary. Keeping one timing per frequency copied one side of the
QSO and silently dropped the other.

**Modes do not arbitrate against each other.** An earlier version dropped a
synced signal that overlapped a better-synced one, on the assumption that only
one of them could be real. That threw away a 32/1000 station with an 8/250
sitting inside its bandwidth, when both decode perfectly alone. The thresholds,
not arbitration after the fact, are what keep a single signal from being
reported twice.

## Compared with fldigi

fldigi's receiver — Pawel Jalocha's code, which this crate's FEC is validated
against bit-for-bit (see [`tools/ref/olivia`](../tools/ref/olivia)) — performs
the same Walsh correlation, but tracks **one** signal that the operator has
tuned to within its search margin (±250 Hz for 8/500).

The difference is entirely in the search. Same detector, run exhaustively across
the band instead of at one place, which is what lets a panorama decode every
conversation at once without anyone touching a dial.
