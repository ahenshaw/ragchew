# The README's screenshots

Seven images, all taken from the running application. They are listed here with
what each is meant to show, so they can be retaken when the interface moves —
and so that whoever retakes one knows which band it was on.

General: dark theme, the window at its default 1280×800 or a little larger, no other windows behind it, and
crop to the window (no desktop, no shadow). PNG.

| file | run | what should be in frame |
|------|-----|-------------------------|
| `panorama.png` | `ragchew --demo` | the whole window, a minute or so in, with a good spread of threads on their rows and at least one QSO tab open on the left. This is the first thing anyone sees of the app, so let the band fill up before taking it. |
| `waterfall.png` | `ragchew --demo` | the panorama only — crop out the toolbar and status bar. Wants a couple of rows that have been nudged apart, so their leader lines are visibly doing their job, and the band bar on the waterfall's right edge in shot. |
| `audio-input.png` | `ragchew` (live) | **☰ ▸ Audio input** open, with the PipeWire capture entry expanded so the *take audio from* submenu shows something real — a browser on a WebSDR is the case worth showing. |
| `settings.png` | any | the **☰** menu open, sliders and all, far enough down to include *forget* and *fade*. |
| `qso.png` | `ragchew --signals` | the QSO panel with two or three tabs, the top one showing filled fields, several logged overs marked RX/TX, the SNR trace drawn, and something half-typed in the reply box. Crop to the panel plus a little of the panorama beside it. |
| `readout.png` | any, with a rig or `rigctld -m 1 -r /dev/null --set-conf=ptt_type=RIG` | the frequency readout close up — digits, the mode beside them, the TX button. Worth one with TX down and red, if the crop still reads clearly. |
| `rig.png` | any | **☰ ▸ Rig** open on Elecraft, showing the port field, the **…** device list and the baud rate. |
| `weak.png` | `ragchew --weak` | let it run to the end of the band, then crop to the panorama: the point is text on six rows against a waterfall with nothing on it that an eye can find. |

The headless renderer (`scene_png`) is not an alternative for any of these — it
draws the layout but has no font rasteriser, so every row of text comes out as a
bar. It is a layout check.
