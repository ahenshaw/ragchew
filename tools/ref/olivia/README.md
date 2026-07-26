# Olivia reference cross-check

Bit-exact ground truth for the Olivia FEC, from Pawel Jalocha's MFSK
implementation — the same code [fldigi](https://github.com/w1hkj/fldigi) uses,
so agreeing with it is what makes a `ragchew` decode of a real signal mean
anything.

`jalocha/` holds the reference headers unmodified, as fetched from fldigi's
`src/include/jalocha/`. They are GPL, like this crate, and are kept here only
for test regeneration and comparison. The parts that matter are:

| file        | what it defines |
|-------------|-----------------|
| `pj_mfsk.h` | `MFSK_Encoder` / `MFSK_SoftDecoder` — the Walsh spreading, scrambling and interleaving |
| `pj_fht.h`  | the Hadamard transform pair, whose sign convention decides which Walsh function a character maps to |
| `pj_gray.h` | the Gray mapping from symbol value to carrier position |

The rest are only there because `pj_mfsk.h` includes them.

## Encoder/decoder vectors (`harness.cc`)

Prints, per mode and message:

    bits_per_symbol|chars(hex)|symbols(64)|tones(64)|decoded(hex)

Build & regenerate `tests/vectors/olivia_vectors.txt`:

    g++ -O2 -std=c++17 -Ijalocha harness.cc -o oliviaref
    ./oliviaref > ../../../tests/vectors/olivia_vectors.txt

`tests/olivia_reference.rs` checks our `olivia::fec` against every line.

## Decode yield on real audio (`decode_harness.cc`)

Runs the reference `MFSK_Receiver` — fldigi's actual receiver — over a WAV and
prints what it copies, so this crate's yield on a real recording can be measured
against it rather than guessed at.

    g++ -O2 -std=c++17 -Ijalocha decode_harness.cc -o oliviadec
    ./oliviadec recording.wav 8 500 1000        # tones, bandwidth, centre Hz

Its tuning tolerance is wide (±250 Hz for 8/500), so the centre only has to be
approximately right. Note that once synced it prints *every* block
unconditionally — it has no per-block confidence gate — so its character count
is always the theoretical maximum and includes whatever the correlation happened
to say. Compare the readable text, not the totals.

## What is *not* pinned down here

Only the FEC. The reference's modulator and demodulator are incremental,
soundcard-driven machinery built around fldigi's audio pipeline, and this crate
does not reimplement them — it does its own block-at-a-time DSP over an audio
buffer. What that DSP has to agree on is the tone layout, and that follows from
the mode parameters alone (see `src/olivia/mode.rs`), with one deliberate
difference: fldigi's first carrier is rounded to an FFT bin, putting its tone
block a quarter of a tone spacing above centre. We centre ours exactly. The
difference is a few Hz, far inside the decoder's tuning tolerance.
