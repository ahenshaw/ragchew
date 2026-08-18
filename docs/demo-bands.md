# The demo bands

Four synthetic bands ship with the app, each built to show one thing. They are
reachable from **Source ▸ Demo bands**, or from the command line:

```
ragchew --demo          # twelve stations, all three protocols
ragchew --weak          # Olivia under the noise floor
ragchew --crowded       # Olivia stacked on top of itself
ragchew --signals       # three QSOs: what each mode costs
```

Every station in them is synthesized by the same encoders the app transmits
with, decoded by the same decoders it listens with, and the numbers below are
measured by `examples/sensitivity.rs` rather than quoted from a mode table. The
demo's tests hold the on-air text to them.

## `--weak`: how far under the noise a signal can be

This is the one that shows what Olivia is *for*: six stations, every one of them
**below the noise floor**, several stacked inside each other's bandwidth, and
one of them carrying no error correction at all.

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

W6GONE is the interesting one among the Olivia stations. It is in the clear,
half a decibel below stations that copy perfectly, and it still falls apart —
because sensitivity tracks *characters per second*, not bandwidth. Every mode
spreads a character over the same 64 chips, so a slower mode spends more energy
on each and hears further: 4/125 at a leisurely 0.98 char/s copies at −12 dB
while 8/500, three times faster, is already breaking up. The FEC does not
degrade gently either — the window between clean copy and nothing at all is
about a decibel wide.

N0FEC makes the opposite point, and is the reason it is there. It is the loudest
station in the band by five decibels and the only one with no error correction,
and those two facts are the same fact. The detector still finds it four decibels
further down, at about −8; what it finds there is a station whose text has
already begun to come apart, with nothing underneath to put it back together.
Every one of those numbers was measured against this band rather than chosen —
−5 dB loses a character, −6 starts swallowing the spaces — and the tests hold
them to it.

## `--crowded`: how many can be in the same place

The same argument from the other end. Where `--weak` asks how far *under* the
noise a signal can be, this asks how many can be in the same place — which is
the more ordinary situation on a real band. Five Olivia modes, all comfortably
above the noise and plainly visible on the waterfall, all read in full:

```
 400 |=========== 32/1000 @900 ===========|
 450   |==== 16/500 @700 ====|
1275              |= 8/250 @1400 =|
1350             |===== 8/500 @1600 =====|
1400              |========== 16/1000 @1900 ==========|
```

Two kilohertz-wide stations meeting at 1400 Hz, one narrower buried completely
inside the first, and two more piled across the seam where they meet — an 8/250
inside *both* wide ones, and an 8/500 lying across that 8/250 and running most
of the way into the second. Nothing here is hard to hear; it is hard to
separate. What separates it is the same 64-chip Walsh coding the weak band
spends on going deep: an interfering station's energy lands across the whole
correlation window instead of in the one place the correlator is looking, so the
neighbours largely fail to register rather than adding their errors together.

Two stations may share a patch of spectrum but not a centre frequency, and that
is a limit of the display rather than of the decoder. Threads are grouped by
protocol and frequency, not by mode, so two Olivia stations closer than the
wider one's association tolerance — 250 Hz for a kilohertz-wide signal — arrive
on one row with both texts woven into it. The decoder reads them both perfectly;
there is simply nowhere to put the second. An earlier version of this band had
the 8/500 dead centre of the 16/1000 and showed exactly that: four threads, one
of them gibberish.

Olivia 4/125 is the one mode absent from it, and for an honest reason. Its four
tones fall on the same 31.25 Hz grid as a 16/500's sixteen, so a 16/500
demodulator centred nearby reads the same station through four of its own tones
and prints a second, garbled copy. The Olivia scan does not arbitrate between
modes on purpose — real bands stack signals, and a scanner that assumes one
signal per slice of spectrum drops the quieter of every such pair — so what
normally prevents double-reporting is the correlation threshold, which covers a
narrow mode locking onto a slice of a wide one but not this, its reverse. 4/125
stays in `--weak`, where at −12 dB it is not strong enough to fake a wider mode.

## `--signals`: what each mode costs

The one to start with if the question is *which mode should I be using*. Three
QSOs run side by side for under three minutes, one per protocol, and every over
states its own speed and how deep its mode hears:

| thread | mode | speed | copies at | loud op | quiet op |
|--------|------|------:|----------:|--------:|---------:|
| 700 Hz  | JS8 Normal | 10 wpm | −15 dB | 100 W | **2 W** |
| 1000 Hz | PSK31      | 47 wpm |  −5 dB | 100 W | **20 W** |
| 1800 Hz | Olivia 8/250 → 16/500 → 32/1000 | 18 → 29 wpm | −13 → −11 dB | 100 W | **5 W** |

Every station in the band is heard over one path, so a hundred watts is a
hundred watts whichever mode it is keyed in and all three loud operators arrive
at the same +6 dB. What differs is what their partners can get away with: each
quiet station is placed exactly four decibels above its own mode's floor, and
the power that takes is 20 W on PSK31, 5 W on Olivia and 2 W on JS8. Ten
decibels between the ends of that, which is the amount of transmitter a mode
with no error correction costs you — and JS8 charges it back at a fifth of
PSK31's typing speed.

The Olivia thread is the one that moves. The two operators start on 8/250, agree
to speed up twice, and finish on 32/1000 — 18, 23 then 29 words a minute against
floors of −13, −12 and −11 dB, so two decibels of reach buys sixty per cent more
speed. All three modes are sent on the same centre frequency, which is both what
operators do (you QSY the mode, not the dial) and what keeps the exchange on one
row: threads are grouped by protocol and frequency and not by mode, so the row's
label simply follows the station up through the gears.

Every over is a separate over, on all three threads, and the three protocols get
there by different routes. JS8 sets a flag on the last frame of an over and the
tracker reads it — that is the `<>` in the text. Olivia and PSK31 carry no such
flag, and no callsign the tracker parses either, so the only cue they leave is
the turnaround, which has to be longer than the silence that ends an over: 8.2
seconds for Olivia, five for PSK31. This band leaves a real one, which is a third
of its length and the reason it runs as long as it does. Its first draft left a
second and a half — no time at all for the other operator to hear you stop and
key up — and both conversational threads arrived as a single unbroken paragraph
with the two operators run together, and one QSO log entry apiece.

Both numbers in that table are measured rather than quoted, by
`examples/sensitivity.rs`. Neither will match a published mode table, for two
reasons worth knowing. Speed is five characters to a word over ordinary prose,
and only Olivia has a fixed rate — the same PSK31 station types at 47 wpm sending
English and 37 sending callsigns, and a JS8 frame swallows 13 characters of prose
but 27 of a string built to suit its varicode. And the floor here is the level at
which a *whole over* comes back character for character, every time, which is a
stricter test than the half-of-all-transmissions convention the published figures
use: JS8 Normal is quoted at −24 dB on that convention and reaches −15 on this
one. A band that gets watched rather than summarised needs the strict number.
