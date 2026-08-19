# The README's screenshots

Eight images, all taken from the running application. They are listed here with
what each is meant to show, so they can be retaken when the interface moves —
and so that whoever retakes one knows which band it was on.

How the current set was taken: dark theme, window 1400×900, captured with
`import -window <id>` on X11 and cropped with ImageMagick. The rig is hamlib's
dummy, which is what puts a dial reading in the frame with no radio on the
bench:

```
rigctld -m 1 -r /dev/null --set-conf=ptt_type=RIG
printf 'F 14078000\nM USB 3000\n' | nc -q1 127.0.0.1 4532
```

14.078 MHz USB because it is the JS8 calling frequency — the readout should say
somewhere a station would actually be.

| file | run | what should be in frame |
|------|-----|-------------------------|
| `panorama.png` | `ragchew --demo` | the whole window at the end of the band, twelve threads on their rows, a QSO open on the left with a reply composed. This is the first thing anyone sees of the app, so let the band fill up before taking it. |
| `waterfall.png` | `ragchew --demo` | the panorama only — toolbar and status bar cropped off. Wants the pair of rows that have been nudged apart so the leader lines are visibly doing their job, and the band bar on the waterfall's right edge in shot. |
| `audio-input.png` | `ragchew` (live) | **☰ ▸ Audio input** open with the PipeWire capture entry expanded, so the *take audio from* submenu names a real application. A browser on a WebSDR is the case worth showing; it has to be playing before the menu is opened, since the submenu lists only what is making noise. |
| `settings.png` | any | the **☰** menu open, sliders and all, far enough down to include *forget*, *fade* and the version at the foot. |
| `qso.png` | `ragchew --signals` | the QSO panel after a few overs: filled fields, RST auto-filled from the measured SNR, several logged overs with their `<>` markers, the SNR trace drawn, and something half-typed in the reply box. Crop to the panel plus a little of the panorama beside it. |
| `readout.png` | any, with a rig or the dummy above | the frequency readout close up — digits, mode, and the TX button red and counting. Capture within a second of moving onto a digit, or the tooltip covers the panel. |
| `rig.png` | any | **☰ ▸ Rig** on the Elecraft segment, showing the port field, the **…** device list and the baud rates. The current shot is *unconnected* ("not tried yet"): with a KX3 on the cable it is worth retaking, so the **…** list shows a real `/dev/serial/by-id/…` and the panel reports a link. |
| `weak.png` | `ragchew --weak` | run to the end of the band, then crop to the panorama: text on six rows against a waterfall carrying one obvious PSK31 carrier and almost nothing else an eye would pick out. |

The headless renderer (`scene_png`) is not an alternative for any of these — it
draws the layout but has no font rasterizer, so every row of text comes out as a
bar. It is a layout check.
