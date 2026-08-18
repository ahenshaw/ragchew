# The library, and the command line

The application is one front end onto a plain Rust library. The library has no
windowing, audio or GUI dependency, depends on `rustfft` and nothing else,
contains no `unsafe`, and can be used on its own — it is the whole of the
signal processing, with the application removed.

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

## Command line

`ragchew_tool` decodes and encodes files without opening a window:

```
cargo run --release --example ragchew_tool -- decode recording.wav
cargo run --release --example ragchew_tool -- encode js8 "CQ CQ DE KN4CRD" out.wav 1500
cargo run --release --example ragchew_tool -- encode olivia:16/500 "CQ DE W1AW K" out.wav 1500
```

WAV must be 12 kHz mono 16-bit PCM; transcode with
`ffmpeg -i in.mp3 -ac 1 -ar 12000 out.wav`.

The application itself will decode a recording straight to CSV and exit, with no
window opened, which is usually the easier route for a night's capture:

```
ragchew night.wav --csv                # writes night.csv
```

## Measuring harnesses

These run on the shipping code rather than a copy of it, which is the point of
them:

```
cargo run --release --example sensitivity          # every mode's speed and reach
cargo run --release --example sensitivity -- psk31 # a few modes, while calibrating one
cargo run --release --example psk_gate             # PSK gate: sensitivity and false alarms
cargo run --release --example snr                  # SNR estimator against known levels
```

## Rendering without a display

The pure rendering pieces build without the windowing or audio stack, so a band
can be looked at on a machine that has no display at all:

```
cargo run -p ragchew-gui --no-default-features --release --example scene_png -- scene.png
cargo run -p ragchew-gui --no-default-features --release --example render_png -- recording.wav out.png
```

`scene_png` takes an optional playback time and demo band name
(`scene.png 45 weak`). It draws the layout — rows, leader lines, waterfall — but
not the text itself: there is no font rasteriser in that build, so a row of text
comes out as a bar the width the text would have been. It is a layout check, not
a screenshot.
