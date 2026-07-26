# ragchew

A panoramic monitor for HAM radio keyboard-to-keyboard digital modes: a
waterfall of a whole audio band, with every conversation in it decoded and
printed inline — whatever mode each one is using.

Two protocol families are implemented, both from scratch in Rust:

| Protocol | Modes | Character |
|----------|-------|-----------|
| **JS8** ([JS8Call](https://js8call.com)) | Slow / Normal / Fast / Turbo | 15-second bursts on a UTC grid, LDPC-coded, packed messages |
| **Olivia** MFSK | 8/250, 16/500, 32/1000, … any `tones/bandwidth` pair | continuous conversational text, Walsh-Hadamard-coded |

They could hardly be less alike — one is a scheduled burst mode with a packed
87-bit payload, the other a free-running character stream with no sync pattern
at all — which is the point: `ragchew` decodes them side by side in one band and
presents them the same way.

## Try it

```
cargo run -p ragchew-gui --release -- --demo          # synthetic multi-protocol band
cargo run -p ragchew-gui --release -- --live          # decode the default audio input
cargo run -p ragchew-gui --release -- recording.wav   # play back a recording
```

The demo band carries eleven stations — all four JS8 submodes and three Olivia
modes — including a JS8 station transmitting *inside* an Olivia signal's
bandwidth at 2000 Hz, which is where the two protocols visibly stay separate
conversations while sharing a patch of spectrum.

The mode menus in the toolbar control what is scanned for; each protocol's
channels are coloured by mode. Scroll to pan, pinch (or ctrl+scroll) to zoom,
shift+scroll to size the text. The frequency axis — scale and band position —
is overlaid on the waterfall's right edge; drag the bar there to move around the
whole band whatever the zoom. Drag the gutter between the text and the waterfall
to give either side more room.

Headless, without the windowing/audio stack — renders straight to PNG:

```
cargo run -p ragchew-gui --no-default-features --release --example scene_png -- scene.png
cargo run -p ragchew-gui --no-default-features --release --example render_png -- recording.wav out.png
```

### Command line

```
cargo run --release --example ragchew_tool -- decode recording.wav
cargo run --release --example ragchew_tool -- encode js8 "CQ CQ DE KN4CRD" out.wav 1500
cargo run --release --example ragchew_tool -- encode olivia:16/500 "CQ DE W1AW K" out.wav 1500
```

(WAV must be 12 kHz mono 16-bit PCM; transcode with
`ffmpeg -i in.mp3 -ac 1 -ar 12000 out.wav`.)

## Library

```rust
use ragchew::protocol;

let (samples, _rate) = ragchew::wav::read("recording.wav").unwrap();
let modes = protocol::default_modes();
for d in protocol::decode_all(&samples, 300.0, 2600.0, &modes) {
    println!("{:7.1} Hz  t={:6.2}s  {:>14}  {}", d.hz, d.time_s, d.mode.name(), d.text);
}
```

`protocol::Decode` is the common currency: some text, at some frequency, at some
time. Adding a protocol means adding a `ModeId` variant and a branch in
`decode_all`; nothing above that layer changes.

Each protocol is also usable directly:

```rust
use ragchew::js8::message::IType;
let audio = ragchew::js8::encode_freetext("HELLO WORLD", IType::None, 1500.0);
assert_eq!(ragchew::js8::decode_near(&audio, 1500.0, 0).unwrap().text(), "HELLO WORLD");

use ragchew::olivia::{self, OL_16_500};
let audio = olivia::encode("CQ DE W1AW K ", 1500.0, OL_16_500);
let text: String = olivia::decode_all_mode(&audio, 1200.0, 1800.0, OL_16_500)
    .iter().map(|r| r.text()).collect();
assert!(text.starts_with("CQ DE W1AW"));
```

## How the two protocols work

**JS8** — free text is Huffman/varicode packed, framed with a type code and a
CRC-12, LDPC(174,87) coded, and sent as 79 8-FSK symbols with three 7-tone
Costas sync blocks:

```
text ──varicode──▶ 72-bit payload ──+itype──▶ 75 bits ──+CRC-12──▶ 87 bits
     ──LDPC(174,87)──▶ 174 bits ──recode──▶ 79 × 8-FSK symbols (3 Costas sync)
     ──continuous-phase FSK──▶ 12 kHz audio
```

Decoding is a spectrogram sync search, FFT tone demodulation with sub-bin
frequency tracking, soft LLRs, and belief-propagation LDPC — with the CRC as
the gate that makes wrong guesses cheap to reject.

**Olivia** — each 7-bit character becomes a whole 64-chip Walsh function (6 bits
choose it, the 7th inverts it), so a character survives even when most of its
chips are wrong. `log2(tones)` characters are scrambled and interleaved together
into one 64-symbol block:

```
text ──7-bit chars──▶ blocks of log2(tones) chars ──Walsh(64) spread──▶
     64 × log2(tones) bits ──scramble + interleave──▶ 64 symbols
     ──Gray──▶ tones ──overlapped raised-cosine MFSK──▶ 12 kHz audio
```

There is no sync pattern to look for and, spread that thin, a signal need not
stand out of the noise by energy at all — so the Walsh correlation *is* the
detector, and the decoder searches every frequency in the band against every
block timing. What makes that affordable is screening each hypothesis on a
single character first, and only paying for a whole block when one survives.

Every timing that syncs is kept, not just the best one per frequency. Two
stations working each other sit within a fraction of a tone spacing of one
another and take turns, each keying up on its own block boundary — so a single
frequency slot routinely carries several block timings, and only *time*
separates the two sides of the QSO.

## Architecture

| Module | Responsibility |
|--------|----------------|
| `protocol` | Protocol-agnostic `ModeId` / `Decode`, and the band-scan dispatcher |
| `dsp` | Shared FFT plan cache and windowed, frequency-shifted transforms |
| `spectrogram` | Display spectrogram (one waterfall serves every protocol) |
| `wav` | Minimal 16-bit PCM mono WAV I/O |
| `js8::varicode` | Free-text Huffman pack/unpack |
| `js8::message` | Frame assembly/parsing (CRC, itype, callsign/grid, dense word list) |
| `js8::crc` | CRC-12 (`0xc06`, JS8 `^= 42`) |
| `js8::ldpc` | LDPC(174,87) encode + sum-product decode |
| `js8::modem` | 8-FSK synthesis; sync search, FFT demod, sub-bin search, soft LLRs |
| `js8::tables` | Generator / parity / Costas constants (from `tools/gen_tables.py`) |
| `olivia::mode` | `tones/bandwidth` parameters and everything derived from them |
| `olivia::fht` | Fast Hadamard transform |
| `olivia::fec` | Walsh spreading, scrambling, interleaving, soft block decode |
| `olivia::modem` | Overlapped MFSK synthesis; tone demod, frequency/timing search |

The GUI (`ragchew-gui`) keeps its pure rendering and layout pieces (`waterfall`,
`layout`, `scene`, `colormap`, `channels`) free of any windowing dependency, so
they are unit-tested and validated by rendering to PNG without a display.

## Performance

Single core, `--release`, on a 12 kHz recording:

| Workload | Audio | Wall | vs real-time |
|----------|------:|-----:|-------------:|
| JS8 encode | 12.6 s/frame | 1.8 ms | ~6,900× |
| JS8 decode 1 cycle, 300–2600 Hz | 15 s | 0.7 s | ~22× |
| JS8 decode full recording, 300–2600 Hz | 134 s | 3.9 s | ~35× |
| Olivia band scan, one mode, 300–2600 Hz | 60 s | 1–3 s | 20–60× |

The JS8 decoder builds one magnitude spectrogram and decodes candidate frames
straight from it, filtering with a cheap hard-decision parity check before
running belief-propagation. For reference, Robert Morris's `fate` decoder
(FFTW + OSD) on the same file runs ~1.9× real-time.

The Olivia scan builds two tone grids per mode (a half-bin apart, so any centre
frequency is covered) and searches them exhaustively; a quiet band is at the
fast end of that range and a busy one at the slow end, since every timing that
looks plausible has to be decoded properly to be ruled out. Cost scales with the
number of modes enabled, which is what the GUI's mode menus are really for.

## Validation

Both protocols are checked bit-for-bit against independent implementations —
see `tools/ref`.

**JS8**, against Robert Morris (AB1HL)'s
[`fate`](https://github.com/rtmrtmrtmrtm/fate):

- `tests/reference.rs` — every stage (message bits, LDPC codeword, 79 symbols,
  unpacked text) is bit-identical to the reference, plus a clean audio
  round-trip and decoding a reference-synthesized WAV.
- `tests/noise.rs` — decodes through additive noise well below the signal level.
- `tests/real_audio.rs` — a real 9-frame off-air recording (John 3:16, drifting
  ~1390 Hz carrier): this crate and the reference decoder both recover **9/9
  frames**, character-identical.
- `tests/multimode.rs` — every submode round-trips and is auto-detected.

**Olivia**, against Pawel Jalocha's reference MFSK implementation (the one
fldigi uses):

- `tests/olivia_reference.rs` — block symbols, the Gray tone mapping, and soft
  decoding are bit-identical to the reference across 4/8/16/32 tones, including
  idle blocks and the top-bit-set characters that ride on an inverted Walsh
  function.
- `tests/olivia.rs` — round-trips at arbitrary frequency and timing offsets,
  full copy at roughly -11 dB SNR (2.5 kHz reference bandwidth), both sides of a
  QSO copied though they share one frequency slot, and — the property the
  decision thresholds exist to protect — **nothing invented** from 45 seconds of
  noise across the whole band, in any mode.
- `tools/ref/olivia/decode_harness.cc` runs the reference receiver over a WAV so
  decode yield on real recordings can be compared directly. On an off-air
  8-minute QSO recording, `ragchew` recovers 94% of the word-forming text the
  reference does while emitting 21% fewer characters overall — it declines to
  guess where the reference prints whatever the correlation happened to say.

```
cargo test --release
```

## License

GPL-3.0-or-later. JS8/JS8Call, fldigi and both reference implementations are
GPL; the embedded `src/js8/jsc_words.txt` word list originates from JS8Call.
