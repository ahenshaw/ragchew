#!/usr/bin/env python3
"""Extract fldigi's PSK31 varicode table into a test vector file.

Unlike the JS8 and Olivia references, there is no algorithm here to run — the
ground truth *is* the table, so this parses it rather than compiling it.

    ./extract.py > ../../../tests/vectors/psk_varicode.txt

fldigi carries the table twice: `varicodetab1` as bit strings for the encoder,
`varicodetab2` as integers for the decoder. They are parsed and compared
against each other, so a disagreement inside the reference itself would be
caught here rather than silently picking one.
"""
import re
import sys
from pathlib import Path

SRC = Path(__file__).parent / "pskvaricode.cxx"

# Latin-1: the comments naming characters 128..255 are in the extended set.
text = SRC.read_text(encoding="latin-1")


def table(name, pattern, convert):
    body = text.split(f"{name}[] = {{", 1)[1].split("};", 1)[0]
    return [convert(m) for m in re.findall(pattern, body)]


encode = table("varicodetab1", r'"([01]*)"', str)
decode = table("varicodetab2", r"0[xX]([0-9A-Fa-f]+)", lambda h: format(int(h, 16), "b"))

for name, tab in (("varicodetab1", encode), ("varicodetab2", decode)):
    if len(tab) != 256:
        sys.exit(f"{name}: expected 256 entries, parsed {len(tab)}")

disagree = [i for i in range(256) if encode[i] != decode[i]]
if disagree:
    for i in disagree:
        print(f"  [{i}] encode={encode[i]} decode={decode[i]}", file=sys.stderr)
    sys.exit(f"fldigi's own two tables disagree on {len(disagree)} entries")

for i, code in enumerate(encode):
    print(f"{i}|{code}")
