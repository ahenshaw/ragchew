# ragchew

A panoramic monitor for HAM radio keyboard-to-keyboard digital modes: a
waterfall of a whole audio band, with every conversation in it decoded and
printed inline — whatever mode each one is using.

Three protocol families are implemented, all from scratch in Rust:

| Protocol | Modes | Character |
|----------|-------|-----------|
| **JS8** ([JS8Call](https://js8call.com)) | Slow / Normal / Fast / Turbo | 15-second bursts on a UTC grid, LDPC-coded, packed messages |
| **Olivia** MFSK | 8/250, 16/500, 32/1000, … any `tones/bandwidth` pair | continuous conversational text, Walsh-Hadamard-coded |
| **PSK** | PSK31, PSK63 | continuous differential BPSK, 31 Hz wide, no error correction at all |

They could hardly be less alike — one is a scheduled burst mode with a packed
87-bit payload, the next a free-running character stream with no sync pattern at
all, the third 31 Hz of carrier with nothing to prove itself with — which is the
point: `ragchew` decodes them side by side in one band and presents them the
same way.

PSK31 is the awkward one, and interesting for it. JS8 has a Costas array, LDPC
and a CRC; Olivia has a Walsh correlation measured against the noise floor. Feed
a BPSK31 demodulator pure noise and it produces bits, and varicode turns two in
five of them into letters — so a monitor that prints everything it scans has to
supply the proof the mode does not carry. `src/psk/modem.rs` documents the
three-stage gate that does it, and `examples/psk_gate.rs` is the harness the
thresholds were measured on.

## Try it

```
cargo run -p ragchew-gui --release -- --demo          # synthetic three-protocol band
cargo run -p ragchew-gui --release -- --weak          # Olivia under the noise floor
cargo run -p ragchew-gui --release -- --crowded       # Olivia stacked on top of itself
cargo run -p ragchew-gui --release -- --live          # decode the default audio input
cargo run -p ragchew-gui --release -- recording.wav   # decode a recording
cargo run -p ragchew-gui --release                    # the same, live: it is the default
```

Run it with no arguments and it starts listening to the default audio input —
monitoring is what it is for. Everything else is reachable from the **Source**
menu: *Open audio file…* for a native file picker, or straight to the demo
bands. `--help` lists the lot.

Window size and position, the panel splits, text size, playback and waterfall
settings, how long text stays on a row, the modes being scanned, the chosen
audio input and output, the palette (dark, light, or following the desktop),
and where in the band the view is pointed all persist between runs
(`~/.local/share/ragchew/app.ron` on Linux). A saved view is fitted to the band
on the way in, so a settings file from another build — or one edited by hand —
cannot reopen the app zoomed into 50 Hz of nothing. Conversations are not saved:
a log is a record, and writing one is a logging program's job.

`--weak` is the one that shows what Olivia is *for*: six stations, every one of
them **below the noise floor**, several stacked inside each other's bandwidth,
and one of them carrying no error correction at all.

| station | mode | SNR (2.5 kHz) | placement | copy |
|---------|------|--------------:|-----------|------|
| W1WIDE | 32/1000 |  −9 dB | host | full |
| K2IN   | 8/250   |  −9 dB | inside W1WIDE's 1 kHz | full |
| W4HOST | 16/500  | −12 dB | host | full |
| N3NARO | 4/125   | −12 dB | inside W4HOST's 500 Hz | full |
| W6GONE | 8/500   | −12.5 dB | in the clear | **fragments** |
| N0FEC  | PSK31   |  −4 dB | in the clear | full |

Every one of those puts less power into the band than the noise does, leaves no
trace an eye can find on the waterfall, and is read anyway — including where two
of them are sitting on top of each other.

The last row is the interesting one. It is in the clear, half a decibel below
stations that copy perfectly, and it still falls apart — because sensitivity
tracks *characters per second*, not bandwidth. Every mode spreads a character
over the same 64 chips, so a slower mode spends more energy on each and hears
further: 4/125 at a leisurely 0.98 char/s copies at −12 dB while 8/500, three
times faster, is already breaking up. The FEC does not degrade gently either —
the window between clean copy and nothing at all is about a decibel wide.

The last row makes the opposite point, and is the reason it is there. N0FEC is
the loudest station in the band by five decibels and the only one with no error
correction, and those two facts are the same fact. The detector still finds it
four decibels further down, at about −8; what it finds there is a station whose
text has already begun to come apart, with nothing underneath to put it back
together. Every one of those numbers was measured against this band rather than
chosen — −5 dB loses a character, −6 starts swallowing the spaces — and the
tests hold them to it.

`--crowded` is the same argument from the other end. Where `--weak` asks how far
*under* the noise a signal can be, this asks how many can be in the same place —
which is the more ordinary situation on a real band. Five Olivia modes, all
comfortably above the noise and plainly visible on the waterfall, all read in
full:

```
 400 |=========== 32/1000 @900 ===========|
 450   |==== 16/500 @700 ====|
1275              |= 8/250 @1400 =|
1400              |========== 16/1000 @1900 ==========|
1650                   |===== 8/500 @1900 =====|
```

Two kilohertz-wide stations meeting at 1400 Hz, one narrower station buried
completely inside each, and an 8/250 sitting across the seam — inside *both*.
Nothing here is hard to hear; it is hard to separate. What separates it is the
same 64-chip Walsh coding the weak band spends on going deep: an interfering
station's energy lands across the whole correlation window instead of in the one
place the correlator is looking, so the neighbours largely fail to register
rather than adding their errors together.

Olivia 4/125 is the one mode absent from it, and for an honest reason. Its four
tones fall on the same 31.25 Hz grid as a 16/500's sixteen, so a 16/500
demodulator centred nearby reads the same station through four of its own tones
and prints a second, garbled copy. The Olivia scan does not arbitrate between
modes on purpose — real bands stack signals, and a scanner that assumes one
signal per slice of spectrum drops the quieter of every such pair — so what
normally prevents double-reporting is the correlation threshold, which covers a
narrow mode locking onto a slice of a wide one but not this, its reverse. 4/125
stays in `--weak`, where at −12 dB it is not strong enough to fake a wider mode.

The toolbar is one line: **☰** holds the display settings, the audio input and
output, *Reset view* and *Quit*; **Source** opens a recording or switches what
is being listened to; the mode
menus on the right control what is scanned for, and each protocol's threads are
coloured by mode. **clear**, under a red ✕, takes every station's text off the
rows.

Everything else is done on the canvas. Scroll to pan, pinch (or ctrl+scroll) to
zoom, shift+scroll to size the text. The frequency axis — scale and band
position — is overlaid on the waterfall's right edge; drag the bar there to move
around the whole band whatever the zoom. Drag the gutter between the text and
the waterfall to give either side more room. Right-click a row to clear that one
station. Point at the waterfall or the band bar and the status bar reads out the
frequency under the cursor — an offset from the dial, like every frequency here
— which is how you find what to tune to before there is any text to click.

The rig's frequency reads out above the conversations, amber on black. Every
digit is its own control: scroll over one and it steps its own decade, from
single hertz at the right to tens of megahertz at the left, and small arrows
appear over and under it for a hand that would rather click. Anywhere else on
the readout a click of the wheel is a hundred hertz.

Click the readout and it takes the keyboard: **←→** move between digits, **↑↓**
step the one you are on, and typing a digit fills it and moves along, so a whole
frequency goes in as eight keystrokes from the left. A typed frequency shows in
a different colour until **Enter** sends it — entered a digit at a time it
passes through others on the way, and any of those is somewhere the radio would
actually go — while **Esc**, or clicking away, puts back whatever the rig says.
Steps made with the wheel or the arrows need no Enter: each one is complete in
itself, and reaching for the mouse mid-entry consolidates what you had typed
and sends it.
Point at the mode beside the digits and the radio's own modes drop out of it —
click one and the rig changes to it. What it shows is what the rig reports —
turn the knob on the radio and the digits follow — except while the wheel is being turned, when they follow the
hand and hand back a second after it stops.

A band left running fills up with conversations that finished an hour ago, so
the window can be told to forget: **☰ ▸ forget** is how long a station's text
stays on its row, and **fade** is how long it takes to go once it is that old.
The oldest text goes first, a piece at a time, so a station still talking keeps
what it has just said and loses only what it said too long ago; a station that
has been quiet for the whole timeout leaves the window altogether. It starts at
fifteen minutes, which is longer than any over and shorter than the gap that
means the QSO ended; zero turns it off and keeps everything. None of this
touches the decoding or the logs: an open conversation keeps every word it
took, cleared or not.

## Working a station

Click a station's text and it opens as a **QSO** in the panel on the left, one
tab per conversation, labelled with the other operator's call as soon as the
text says what it is. A tab carries the usual information — call, name, QTH,
grid, signal reports — over a log of the exchange, each line stamped with how
far into the QSO it landed and marked `RX` or `TX`. The log fills itself: a QSO
follows the thread it was opened on, so whatever that station decodes next
appears whether or not its tab is the one on top. **＋** opens a blank tab for a
station not heard yet.

Under the log is a reply box, one per QSO, so you can leave a half-written
transmission in one tab and come back to it. **Send** modulates what is in it at
the QSO's carrier and mode and plays it out of the chosen audio output. Olivia
goes immediately, being continuous; JS8 waits for the next boundary of its
submode's UTC cycle, and a message too long for one frame goes out as several,
one per cycle. The audio goes to the device you point it at, which is all a rig
on a USB codec (or VOX) needs.

An **Elecraft** KX3 or K3 needs no daemon at all: **☰ ▸ Rig ▸ Elecraft**, the
serial port the cable is on, and the rate the radio's menu is set to (38400
from the factory). The **…** beside the port lists what the machine says it has
— `/dev/serial/by-id/…` on Linux, named after the adapter rather than the order
it enumerated in, `cu.` devices on macOS, `COM` ports on Windows — and the field
stays typeable for a port that list does not know about. It keys, tunes, reads
the frequency and mode, and reads back whether the radio is transmitting — so a
hand on the radio's own PTT shows on the TX button the same as one on the app's.

A **Yaesu** is the same deal on the same two settings: **☰ ▸ Rig ▸ Yaesu**, for
an FT-991A, FT-891, FT-710, FTDX10, FTDX101, and back through the FT-450D and
FTDX3000. The rate is menu 031, `CAT RATE`, and these leave the factory at 4800
rather than an Elecraft's 38400. One thing it does not do: show the receiver's
filter, or shade the waterfall outside it. Yaesu reports the filter as an index
into a per-mode table rather than a width in hertz, and there is no honest way
to turn one into the other — so it is left unanswered instead of guessed at.
The older FT-817 and FT-857 speak a different, binary CAT and are not this.

For any other rig, **☰ ▸ Rig** points the app at hamlib's `rigctld`
— run `rigctld -m <model> -r <device>` and give it the address. Hamlib will not
key a rig it has no PTT method for, and says so with "not available on this
rig"; `--set-conf=ptt_type=RIG` (or `RTS`, or `DTR`) is what it is asking for.
That applies to hamlib's own dummy too, which is the cheapest way to watch the
whole path work without a radio on the bench:
`rigctld -m 1 -r /dev/null --set-conf=ptt_type=RIG`. The **TX**
button beside the frequency readout then keys and unkeys by hand, for tuning up
or checking into a dummy load, and goes red for as long as the rig is transmitting, with a
count of how long it has been down. It goes amber instead when the rig is
transmitting for a reason the app did not ask for — a hand on the front panel, a
foot switch, a line stuck down — because that is worth telling apart from your
own key-down. The rig is asked what it is doing twice a second rather than
assumed — and a rig that cannot answer, which is any station keying over RTS
with no CAT read-back, is asked once and then left alone rather than treated as
broken. Nothing keys unless you configure it: the default is no keying at all.

**Send** keys the rig itself once one is configured: the key goes down a
tenth of a second before the audio and comes up a tenth after it, and a JS8
message that runs to several frames gets a key-down *each*, since the silence
between frames is seconds long and a key held across it is a bare carrier. If
the rig is not answering, the transmission does not go out at all and the
status bar says why — audio into a rig that did not key is silence at best, and
on a station left on VOX it is a transmission nobody asked for.

A key-down is always followed by a key-up. One is forced if a transmission
outlasts its limit, if the app exits, or if the thread doing the keying comes
apart — the last of those by unwinding through the code that unkeys, since that
is the only mechanism a panic cannot skip.

Each directly-driven radio is its own feature, so a build carries only the
protocols it will use. `elecraft` and `yaesu` are both on by default; `serial`
is the port underneath and `cat` the command language the two share, turned on
by whichever rig features are rather than chosen directly.

```
cargo build --release                                              # rigctld + both radios
cargo build --release -p ragchew-gui --no-default-features \
    --features desktop,yaesu                                       # rigctld + Yaesu
cargo build --release -p ragchew-gui --no-default-features \
    --features desktop                                             # rigctld only
```

`rigctld` is always built — it is one protocol for every rig hamlib knows, so
there is nothing to choose. A build without a given radio reads a settings file
that names it, keys nothing, and keeps the port and rate for the next build that
has it.

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

[**docs/olivia-decoder.md**](docs/olivia-decoder.md) walks through the whole
receive chain: the tone grid, the search, how the decision thresholds are
calibrated, and the two bugs that only a real off-air recording exposed.

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
running belief-propagation. For reference, Robert Morris's `fate` decoder
(FFTW + OSD) on the same file runs ~1.9× real-time.

The Olivia scan builds two tone grids per mode (a half-bin apart, so any centre
frequency is covered) and searches them exhaustively; a quiet band is at the
fast end of that range and a busy one at the slow end, since every timing that
looks plausible has to be decoded properly to be ruled out. Cost scales with the
number of modes enabled, which is what the GUI's mode menus are really for.

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
  full copy at roughly -11 dB SNR (2.5 kHz reference bandwidth), both sides of a
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
  would be a plausible wrong character for ever with nothing downstream to
  catch it — and the structural tests do not catch it either.
- `tests/psk.rs` — both modes round-trip, PSK31 and PSK63 do not invent each
  other, one station stays one carrier, and — the property the three-stage gate
  exists to protect — **nothing invented** from noise, nor from a band
  containing only JS8 and Olivia.
- `examples/psk_gate.rs` — the sensitivity and false-alarm harness the gate's
  thresholds were measured on, running on the shipping code rather than a copy
  of it. 0 of 55,320 noise candidates clear the first two stages.

```
cargo test --release
```

## License

GPL-3.0-or-later. JS8/JS8Call and fldigi are GPL, as is Pawel Jalocha's MFSK
reference vendored in `tools/ref/olivia/jalocha/`; the embedded
`src/js8/jsc_words.txt` word list originates from JS8Call. The JS8 reference in
`tools/ref/js8/` is Robert Morris's `fate`, which is **MIT** — its licence text
travels with it as `LICENSE.fate`, as MIT requires.
