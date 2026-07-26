# Design spec: panoramic multi-channel monitor

Status: **draft** · Scope: receive/monitor UI · Depends on: the `ragchew` codec
crate

## 1. Goal

A single-screen "panorama" that shows an entire ~4 kHz audio band at once and
decodes **every** conversation in it simultaneously, in whatever mode each one
is using — not a select-one-and-decode tuner. Each signal's decoded text is
shown spatially anchored to that signal's frequency, so you can read many QSOs
at a glance and see where the channels are.

Non-goals (for this spec): transmit, logging/QSO management. See §12.

## 2. Guiding principle

**Decoding is always full-band; the UI is only a view over it.** Every cycle we
decode the whole band with `protocol::decode_all`. Zoom, pan, and text-size are
*display-only* transforms — they never change what gets decoded. A channel
scrolled out of view is still decoded and still accumulates text; it's just not
currently on screen. This keeps the "see all conversations" promise intact
regardless of how the user has framed the view.

## 3. Layout

Three regions, left to right. The **only** shared axis is vertical (frequency);
horizontal is independent on each side.

```
│              TEXT PANEL (wide)             │strip│  WATERFALL │
│                                            │     │  (narrow)  │  freq ▲
│   ...GAVE HIS ONE AND ONLY SON, SO THAT ───┼──╮  │            │
│                                            │  ╰──┼─▓▓▓▓▓▓ 1390 │
│              ...CQ CQ DE W1AW EM73 ────────┼─────┼─▓▓▓▓▓  700  │
│        ...HELLO DE VE3XYZ (nudged up) ──╮  │     │            │
│                                         ╰──┼──╮  │            │
│                                            │  ╰──┼─▓▓▓▓▓  680  │
└────── older ◀──── newer ───────────────── NOW ──┼── newer ▶ older
```

- **Text panel** (wide): decoded text, one row per channel at its frequency.
  Newest words sit at the right edge (against the strip); history ages leftward.
- **Connector strip** (thin, right edge of the text panel): holds the
  horizontal–diagonal–horizontal **leader lines** that link each text row to its
  channel's live position in the waterfall.
- **Waterfall** (narrow): magnitude spectrogram. Newest column at the **left
  edge**; columns age off to the **right**.

The **strip/waterfall seam is "now."** Waterfall time increases to the right;
text time increases to the left; both diverge from the seam. Because the
waterfall's live edge is at the seam, leader lines are short and point at the
freshest energy.

## 4. Coordinate model

- **Vertical (shared): frequency.** A viewport `[f_lo, f_hi] → [0, H]` px, with
  pan and zoom (§7). Linear Hz. Identical transform in both panels so rows line
  up exactly.
- **Waterfall horizontal: time.** `x = 0` (left) is now; `x` increases into the
  past. Visible span is user-set (§8). px/second = waterfall_width / span.
- **Text horizontal: words.** Right edge is now; text flows leftward as it ages.
  Extent is a scrollback buffer, depth user-set (§8).

## 5. Codec interface (already built)

Per decode cycle the UI consumes, for every signal found in the band:

```rust
// from ragchew::protocol
pub struct Decode { pub mode: ModeId, pub hz: f64, pub time_s: f64,
                    pub quality: f32, pub text: String }
pub fn decode_all(samples: &[f32], hz_lo: f64, hz_hi: f64, modes: &[ModeId]) -> Vec<Decode>;
```

This is deliberately protocol-blind: a JS8 frame and an Olivia FEC block arrive
as the same kind of thing, and the UI never branches on which produced what. It
only asks the `ModeId` for what it needs to draw — bandwidth, association
tolerance, how long a chunk took on the air, what colour to use.

A full 4 kHz JS8 cycle decodes in ~0.7 s (see `README.md` §Performance), so
per-cycle live decoding has ample headroom. Olivia costs roughly a second per
mode per minute of audio, which is why the toolbar lets the operator choose
what to listen for.

The UI also needs the **magnitude spectrogram** for the waterfall. The codec
already computes one internally (`Spectro`); we should expose a public,
reusable spectrogram type (bins × frames of magnitude) so the waterfall and the
decoder share one FFT pass rather than each computing their own.

> **Action item:** promote `modem::Spectro` (or a slim variant) to public API:
> `fn spectrogram(&[f32], hz_lo, hz_hi, hop) -> Spectrogram` returning
> `{ bin_lo, n_bins, hop, frames: Vec<Vec<f32>> }`.

## 6. Channels (conversation threading)

A **channel** is a signal tracked across cycles, so a station's text stays on
one row over time even as it drifts.

```rust
struct Channel {
    id: u64,
    hz: f64,            // current (latest) carrier, updated each cycle
    hz_history: Vec<(Instant, f64)>, // for drawing the drifting streak/leader end
    text: String,       // accumulated decoded text (scrollback)
    last_heard: Instant,
    row_y: f32,         // assigned screen y after nudge layout (§9)
}
```

- **Association:** a new `Decode` joins the nearest existing channel of the same
  protocol within a frequency tolerance (at least ±15 Hz — John 3:16 drifted
  ~10 Hz over 2 min — and more for wide modes), else it starts a new channel.
  Protocols never share a channel, so a narrow JS8 station transmitting inside
  a wide Olivia signal's bandwidth stays its own conversation.
- **Text accumulation:** append the chunk text; JS8 frames carry the `<>`
  end-of-over marker to delimit turns, Olivia just runs on.
- **Expiry:** channels idle beyond a timeout fade and are dropped (configurable).

## 7. Frequency zoom / pan (density lever #1)

The shared vertical viewport is pan+zoomable.

- **Zoom out** → whole 4 kHz for the overview; **zoom in** → a pileup spreads
  apart until text rows fit.
- Waterfall rendering under zoom: when zoomed **in**, map/interpolate the visible
  bins across more pixels; when zoomed **out** past 1 px/bin, **max-pool** bins
  per pixel so strong signals don't alias away.
- **Off-screen channels** (above `f_hi` or below `f_lo`) are clipped; show a
  small count at the top/bottom edges (e.g. "▲ 3 more") since they're still being
  decoded.
- Layout (§9) recomputes in screen space at the current zoom, so leader diagonals
  stay short when zoomed in.

## 8. Time windows (user-set)

Two independent controls:

- **Waterfall span** (seconds visible): sets the spectrogram ring-buffer length
  and px/second. Default ~30–60 s. Note: a narrow panel + long span compresses
  each 12.6 s frame into a short streak — there's a practical sweet spot.
- **Text history depth**: how far back the text panel scrolls. Cheap; can be
  "entire session" with scrollback.

## 9. Text panel + leader lines

Each channel gets a single-line text row placed at its frequency's `y`. When
rows would overlap, they're nudged apart and the leader line absorbs the offset.

**Placement algorithm (per frame, in screen space):**
1. Take channels whose `hz` is within the viewport; compute each ideal `y` from
   the shared transform.
2. Sort by `y`; greedily assign final `row_y` enforcing a minimum spacing equal
   to the **current text line-height** (§10). The least-crowded channel keeps its
   ideal `y` ("center channel" rule); neighbors are pushed up/down minimally.
3. Draw each row's text with its newest word at the strip edge, older text
   leftward (clipped to history depth).
4. **Leader line** in the strip: horizontal from the text's right edge at
   `row_y`, diagonal to the channel's live frequency `y_live`, short horizontal
   into the waterfall's left edge. `y_live` tracks drift, so lines move to keep
   up.

Stability: recompute only when the channel set or viewport changes, and animate
`row_y` transitions so text doesn't jitter frame-to-frame.

## 10. Text-size zoom + auto-fit (density lever #2)

Independent of frequency zoom: shrinking the font shrinks each label's required
vertical space (the min-spacing in §9) **without changing the visible band**, so
you can keep the whole 4 kHz on screen and still fit more rows. Smaller text also
widens the visible history horizontally.

- **Legibility floor** ~8–9 px: text zoom helps down to there, then frequency
  zoom / steeper leaders take over. Three levers compose: frequency zoom (spacing
  in px), text zoom (space each label needs), nudge+leaders (residual).
- **Auto-fit mode (default):** for the current visible band, pick the largest
  text size that fits all visible channels without overlap, down to the floor,
  then start nudging. **Manual override** available.

**Interaction bindings (proposed):** wheel = frequency zoom · Ctrl-wheel = text
size · drag = vertical pan · (waterfall span & history depth via settings).

## 11. Timing reality (bake into UX)

The two protocols deliver text on completely different rhythms, and the UI has
to make both feel intentional.

**JS8 is framed, not continuous.** In Normal mode a station sends one ~13-char
frame per **15 s** cycle and it can't be decoded until the frame completes and is
LDPC-decoded. So:

- Text **arrives in ~13-char quanta every 15 s per station**, in a burst just
  after each cycle boundary — the scroll *animates* smoothly but words *land*
  chunked. Design it to feel intentional (e.g. a subtle "just decoded" highlight
  on the new chunk), not laggy.
- Decode latency after a boundary is ~0.7 s wideband — effectively instant.

**Olivia is continuous, and on no schedule at all.** A station keys up whenever
it likes and types; text lands one FEC block at a time — 3 to 5 characters every
one to two seconds, depending on mode. There is no cycle boundary to trigger on,
so the live decoder re-scans a rolling window instead and dedupes what it has
already reported. That makes Olivia text arrive in smaller, more frequent
quanta than JS8's, and adds a few seconds of latency the operator cannot tune
away — the price of a mode with nothing to sync to.
- The decode loop is **cycle-aligned** to the system clock (:00/:15/:30/:45):
  buffer audio continuously, run one `decode_all` over the last ~15 s at each
  boundary, associate results to channels, append text.

## 12. Modes & future

- **Done:** all four JS8 submodes decoded simultaneously, each signal's mode
  detected per-signal rather than configured; the common Olivia modes likewise.
- Every mode scanned costs time, so what to listen for is an operator control
  (the toolbar mode menus), not a fixed list. Defaulting to everything is right
  for a monitor; a busy machine watching one calling frequency should narrow it.
- Later: transmit, per-channel focus/mute, click-a-streak-to-pin, logging,
  waterfall color themes, persistence of the session, more protocols (the
  `protocol` layer exists so that is an additive change).

## 13. Architecture

```
 radio audio ──▶ [capture thread]           (cpal, 12 kHz mono, ring buffer)
                     │
                     ├──▶ [waterfall builder]  hop-FFT columns ──▶ spectrogram ring
                     │
 cycle boundary ────▶ [JS8 worker]     decode_all over the finished cycle
 every few seconds ─▶ [Olivia worker]  decode_all over a rolling window
                     │                     └─▶ Channel model (associate, append)
                     ▼
                 [egui UI thread]  reads spectrogram ring + channel model,
                                   renders waterfall texture + text + leaders
```

- **Crates:** `ragchew` (done) · `eframe`/`egui` (UI) · `cpal` (audio) · the
  `ragchew-gui` workspace member wiring them, so the library stays UI-free.
- **Threading:** capture and decode off the UI thread; the UI reads snapshots
  (channel list + spectrogram ring) behind a mutex/`arc-swap`. Decode of a cycle
  (~0.7 s) must never block rendering.
- **Offline mode:** the same pipeline driven from a WAV file (as `examples/
  ragchew_tool decode` already does) for development without a radio.

## 14. Data model (sketch)

```rust
struct AppState {
    settings: Settings,          // spans, band range, auto-fit, colors
    viewport: Viewport,          // f_lo, f_hi (freq zoom/pan)
    text_px: f32,                // text-size zoom (auto or manual)
    spectrogram: SpectroRing,    // magnitude columns, newest-left
    channels: Vec<Channel>,      // tracked signals + text
}
struct Viewport { f_lo: f64, f_hi: f64 }
struct Settings { band: (f64,f64), waterfall_span_s: f32, history_depth: Depth,
                  auto_text_fit: bool, /* color map, thresholds ... */ }
```

## 15. Build milestones

1. **Offline core:** WAV → spectrogram ring + `decode_all`; static waterfall
   (newest-left) with the shared frequency axis. No text yet.
2. **Channels + text panel:** association/tracking, per-row text at frequency,
   basic (un-nudged) placement.
3. **Leader lines + nudge layout + off-screen indicators.**
4. **Frequency zoom/pan + text-size zoom + auto-fit** with the interaction
   bindings.
5. **Live audio (cpal) + cycle-aligned decode loop** and the "just decoded"
   animation.
6. **Polish:** color/scaling controls, expiry/fade, settings UI.

## 16. Open questions

- Exact channel-association tolerance and drift model (fixed ±Hz vs. velocity
  tracking)?
- Behavior when a station's text row would need to nudge past a screen edge.
- Waterfall color map + magnitude scaling (fixed vs. auto-gain / AGC).
- Should pinning a channel (click) lock its row and highlight its streak?
