# JS8 reference cross-check

Bit-exact ground truth from Robert Morris (AB1HL)'s independent JS8
implementation ("fate": https://github.com/rtmrtmrtmrtm/fate), used to
validate this crate's encoder/decoder. The sources here are fate's, unmodified
and complete as of upstream HEAD, and kept only for test regeneration and
comparison; `harness.cc` and `decode_harness.cc` are this repository's.

fate is **MIT** licensed, not GPL — `LICENSE.fate` is its licence text, which
MIT requires to travel with the code. Some of it is modelled on WSJT-X (the
LDPC tables in `arrays.h`, the encoder in `osd.cc`) and says so in comments,
but the licence on what is vendored here is MIT. This crate being
GPL-3.0-or-later is unaffected: MIT is compatible with it.

## Encoder vectors (`harness.cc`)

Links the reference pack/unpack/ldpc/crc and prints, per message:

    KIND|itype|text|a87(87 bits)|a174(174 bits)|a79(79 symbols)|unpacked

Build & regenerate `tests/vectors/reference_vectors.txt`:

    g++ -O2 -std=c++17 -I. harness.cc pack.cc unpack.cc libldpc.cc -o js8ref
    ./js8ref > ../../../tests/vectors/reference_vectors.txt   # run where words.txt lives

## Reference decoder (`decode_harness.cc`)

Slides a 15 s window over a WAV file and runs the reference `FT8::go()`
decoder (FFTW + OSD) on each cycle, printing every decode. Used to compare
decode yield against this crate on real recordings.

    g++ -O2 -std=c++17 -I. decode_harness.cc js8.cc fft.cc osd.cc libldpc.cc \
        unpack.cc common.cc util.cc -lfftw3 -lpthread -lsndfile -o js8dec
    ./js8dec recording.wav        # run where words.txt lives

Needs `libfftw3-dev` and `libsndfile1-dev`. Both build lines above are checked
to work and the encoder one reproduces the committed
`tests/vectors/reference_vectors.txt` byte for byte.

### Its `snr=` column is not a reference for ours

Tempting, and wrong. `guess_snr` in `js8.cc` takes the strongest tone of each
symbol as signal and the middle tones as noise, scales from a 2.7 Hz noise
bandwidth to 2500 Hz — and then does `snr += 5; snr *= 1.4;`. Multiplying a
figure that is already in dB is a curve fit to whatever WSJT-X prints, not a
measurement, so the column is a *mimic of WSJT-X's readout* rather than a
physical ratio. Its noise term is the other tones of the same symbol, which on
a strong signal are that signal's own leakage, so it also saturates: on
`tests/vectors/john_3_16.wav` it reports −4 to −9 dB for frames that are plainly
far stronger than that.

For the record, measured on that recording three ways: from a genuinely silent
stretch of it (the second before the first frame), **+36.7 dB**; from the
spectrum either side of the signal while it transmits, which is what
[`dsp::snr_db`](../../../src/dsp.rs) does, **+30.4 dB**; from fate, −5 dB. The
6 dB between the first two is real and is the estimator being conservative —
during a transmission this strong the band around it lifts, partly from the
receiver's own images, which fate itself detects at 1147 and 1626 Hz. What
`decode_harness.cc` is genuinely good for is what the section above says:
comparing *decode yield*.

## Regenerate the LDPC / Costas tables

`../../gen_tables.py` parses `arrays.h` + `pack.cc` here into `src/js8/tables.rs`.
