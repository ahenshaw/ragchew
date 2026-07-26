# JS8 reference cross-check

Bit-exact ground truth from Robert Morris (AB1HL)'s independent JS8
implementation ("fate": https://github.com/rtmrtmrtmrtm/fate), used to
validate this crate's encoder/decoder. Reference sources are GPL (WSJT-X
derived) and kept here only for test regeneration and comparison.

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

Needs `libfftw3-dev` and `libsndfile1-dev`.

## Regenerate the LDPC / Costas tables

`../../gen_tables.py` parses `arrays.h` + `pack.cc` here into `src/js8/tables.rs`.
