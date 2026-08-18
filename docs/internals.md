# How it works

Three protocol families, all implemented from scratch in Rust, with no shared
ancestry and almost nothing in common above the sample rate.

| Protocol | Modes | Character |
|----------|-------|-----------|
| **JS8** ([JS8Call](https://js8call.com)) | Slow / Normal / Fast / Turbo | 15-second bursts on a UTC grid, LDPC-coded, packed messages |
| **Olivia** MFSK | 8/250, 16/500, 32/1000, … any `tones/bandwidth` pair | continuous conversational text, Walsh-Hadamard-coded |
| **PSK** | PSK31, PSK63 | continuous differential BPSK, 31 Hz wide, no error correction at all |

## JS8

Free text is Huffman/varicode packed, framed with a type code and a CRC-12,
LDPC(174,87) coded, and sent as 79 8-FSK symbols with three 7-tone Costas sync
blocks:

```
text ──varicode──▶ 72-bit payload ──+itype──▶ 75 bits ──+CRC-12──▶ 87 bits
     ──LDPC(174,87)──▶ 174 bits ──recode──▶ 79 × 8-FSK symbols (3 Costas sync)
     ──continuous-phase FSK──▶ 12 kHz audio
```

Decoding is a spectrogram sync search, FFT tone demodulation with sub-bin
frequency tracking, soft LLRs, and belief-propagation LDPC — with the CRC as the
gate that makes wrong guesses cheap to reject.

## Olivia

Each 7-bit character becomes a whole 64-chip Walsh function (6 bits choose it,
the 7th inverts it), so a character survives even when most of its chips are
wrong. `log2(tones)` characters are scrambled and interleaved together into one
64-symbol block:

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

[**olivia-decoder.md**](olivia-decoder.md) walks through the whole receive
chain: the tone grid, the search, how the decision thresholds are calibrated,
and the two bugs that only a real off-air recording exposed.

Every timing that syncs is kept, not just the best one per frequency. Two
stations working each other sit within a fraction of a tone spacing of one
another and take turns, each keying up on its own block boundary — so a single
frequency slot routinely carries several block timings, and only *time*
separates the two sides of the QSO.

## PSK31

The awkward one, and interesting for it. JS8 has a Costas array, LDPC and a CRC;
Olivia has a Walsh correlation measured against the noise floor. Feed a BPSK31
demodulator pure noise and it produces bits, and varicode turns two in five of
them into letters — so a monitor that prints everything it scans has to supply
the proof the mode does not carry. `src/psk/modem.rs` documents the three-stage
gate that does it, and `examples/psk_gate.rs` is the harness the thresholds were
measured on.

## Architecture

Everything below is Rust, and the dependency list is deliberately short enough
to read in full. The decode library depends on `rustfft` and nothing else, and
contains no `unsafe`. The application adds the windowing, audio and dialog
stacks (`eframe`, `cpal`, `rfd`), `clap`, `serde`, and `egui-elegance` for
widget styling — plus `libc` on Unix and `windows-sys` on Windows, which each
platform's other dependencies already pull in. The only `unsafe` in the
workspace is in `gui/src/rig/serial/`, handing a `termios` or a `DCB` to the
operating system; the serial port is written by hand rather than taken from a
crate, because it is one struct and four calls per platform.

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

The GUI (`ragchew-gui`) keeps its pure rendering, layout and conversation pieces
(`waterfall`, `layout`, `scene`, `colormap`, `channels`, `qso`) free of any
windowing dependency, so they are unit-tested and validated by rendering to PNG
without a display. `audio` and `tx` are the sound card either way round, and the
whole interface lays itself out headlessly in a test — egui is pure layout until
something rasterises it — so the panels are exercised in CI even though the app
cannot run there.

Widget styling comes from [`egui-elegance`](https://crates.io/crates/egui-elegance):
one installed theme that the egui built-ins inherit, plus its own buttons,
sliders, inputs and indicators. Two palettes are used, a matched dark/light pair
(`slate` / `frost`), picked by the Theme setting. Everything ragchew draws
itself — the waterfall, the band bar, the inline text, the QSO tabs — is painted
against `egui::Visuals`, so it follows the installed palette without going
through the crate.

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
running belief-propagation. For reference, Robert Morris's `fate` decoder (FFTW
+ OSD) on the same file runs ~1.9× real-time.

The Olivia scan builds two tone grids per mode (a half-bin apart, so any centre
frequency is covered) and searches them exhaustively; a quiet band is at the
fast end of that range and a busy one at the slow end, since every timing that
looks plausible has to be decoded properly to be ruled out. Cost scales with the
number of modes enabled, which is what the GUI's mode menus are really for.

A debug build is not usable for live audio: one Olivia pass over its 24-second
window across six modes measured 41 s unoptimised against 3.5 s optimised, and
rendering one 45-second waterfall view measured 177 ms a frame. Both crates are
therefore optimised in `dev` too — see the profile section of the root
`Cargo.toml` for the numbers and the trade.

## Validation

All three protocols are checked bit-for-bit against independent implementations
— see `tools/ref`.

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
  full copy at roughly −11 dB SNR (2.5 kHz reference bandwidth), both sides of a
  QSO copied though they share one frequency slot, and — the property the
  decision thresholds exist to protect — **nothing invented** from 45 seconds of
  noise across the whole band, in any mode.
- `tools/ref/olivia/decode_harness.cc` runs the reference receiver over a WAV so
  decode yield on real recordings can be compared directly. On an off-air
  8-minute QSO recording, `ragchew` recovers 94% of the word-forming text the
  reference does while emitting 21% fewer characters overall — it declines to
  guess where the reference prints whatever the correlation happened to say.

**PSK**, against fldigi's varicode table:

- `tests/psk_reference.rs` — all 95 printable codes are digit-for-digit
  fldigi's, in both directions, and the 161 this crate leaves out are confirmed
  not to alias onto ones it prints. Varicode is a hand-assigned table rather
  than an algorithm, and the mode has no FEC or CRC, so a single mistyped digit
  would be a plausible wrong character for ever with nothing downstream to catch
  it — and the structural tests do not catch it either.
- `tests/psk.rs` — both modes round-trip, PSK31 and PSK63 do not invent each
  other, one station stays one carrier, and — the property the three-stage gate
  exists to protect — **nothing invented** from noise, nor from a band
  containing only JS8 and Olivia.
- `examples/psk_gate.rs` — the sensitivity and false-alarm harness the gate's
  thresholds were measured on, running on the shipping code rather than a copy
  of it. 0 of 55,320 noise candidates clear the first two stages.
- `examples/sensitivity.rs` — every mode's speed and reach on one yardstick,
  measured through the same `protocol::decode_all` the application runs, at two
  criteria: where every over of eight copies exactly, and where half do. Where
  the [demo bands](demo-bands.md)' numbers come from; name modes on the command
  line to re-measure a few while calibrating one.

```
cargo test --release
```

Three platforms and every feature combination are built and tested on each push
— see `.github/workflows`. Two thirds of the platform code (the Windows and
macOS serial backends, the whole Yaesu driver) has a compiler and a test bench
behind it and nothing else, which is exactly why the matrix is there.
