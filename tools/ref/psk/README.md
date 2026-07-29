# PSK31 varicode cross-check

Ground truth for the PSK31 varicode table, from
[fldigi](https://github.com/w1hkj/fldigi)'s `src/psk/pskvaricode.cxx` — the same
code most of the PSK31 on the air is sent and received with, so agreeing with it
is what makes a `ragchew` decode of a real signal mean anything.

`pskvaricode.cxx` is that file unmodified, as fetched from fldigi's master
branch. It is GPL, like this crate, and is kept here only for test regeneration
and comparison.

This reference is a different shape from the JS8 and Olivia ones next door.
Those needed a C++ harness built against the reference implementation, because
what had to be checked was an *algorithm*. Varicode has no algorithm: it is 256
codes somebody assigned by hand, so the table is the ground truth and a parse is
the whole job.

## Regenerating the vectors

    ./extract.py > ../../../tests/vectors/psk_varicode.txt

Each line is `ascii_code|bits`, all 256 of them. fldigi carries the table twice
— `varicodetab1` as bit strings for its encoder, `varicodetab2` as integers for
its decoder — and `extract.py` parses both and compares them, so a disagreement
inside the reference itself fails loudly here instead of being resolved by
whichever one happened to be parsed. They agree today.

`tests/psk_reference.rs` checks `psk::varicode` against every line: the 95
printable codes digit for digit in both directions, and the other 161 for the
weaker property that leaving them out does not alias them onto a character we
do print.

## Why this is worth a reference at all

PSK31 has no FEC and no CRC. A single wrong digit in the table is not a decode
failure — it is a plausible wrong character, for ever, on every contact, and
nothing downstream of the table can tell. The structural tests in
`src/psk/varicode.rs` do not catch it either: corrupt one code so that it is
still well-formed and still unique, and all nine of them pass.

## What is *not* pinned down here

Only the table. The modulator, the demodulator and the three-stage detector
gate in `src/psk/modem.rs` are this crate's own, measured rather than
cross-checked — see `examples/psk_gate.rs` for the harness their thresholds come
from.
