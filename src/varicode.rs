//! JS8 free-text Huffman ("varicode") packing, from JS8Call's `varicode.cpp`.
//!
//! A free-text frame carries up to 72 bits: a `10` type marker followed by a
//! prefix-free Huffman code per character, then a `0` and `1`-padding.

/// (character, code) pairs. Order matters for decode: the table is scanned in
/// order and the first fully-matching prefix wins.
pub const HUFFMAN: &[(char, &str)] = &[
    (' ', "01"),
    ('E', "100"),
    ('T', "1101"),
    ('A', "0011"),
    ('O', "11111"),
    ('I', "11100"),
    ('N', "10111"),
    ('S', "10100"),
    ('H', "00011"),
    ('R', "00000"),
    ('D', "111011"),
    ('L', "110011"),
    ('C', "110001"),
    ('U', "101101"),
    ('M', "101011"),
    ('W', "001011"),
    ('F', "001001"),
    ('G', "000101"),
    ('Y', "000011"),
    ('P', "1111011"),
    ('B', "1111001"),
    ('.', "1110100"),
    ('V', "1100101"),
    ('K', "1100100"),
    ('-', "1100001"),
    ('+', "1100000"),
    ('?', "1011001"),
    ('!', "1011000"),
    ('"', "1010101"),
    ('X', "1010100"),
    ('0', "0010101"),
    ('J', "0010100"),
    ('1', "0010001"),
    ('Q', "0010000"),
    ('2', "0001001"),
    ('Z', "0001000"),
    ('3', "0000101"),
    ('5', "0000100"),
    ('4', "11110101"),
    ('9', "11110100"),
    ('8', "11110001"),
    ('6', "11110000"),
    ('7', "11101011"),
    ('/', "11101010"),
];

/// Pack `text` into the 72-bit free-text payload (type marker `10` at bits 0..2,
/// then Huffman codes, then a `0` and `1`-padding). Returns the payload and the
/// number of input characters consumed (may be fewer than supplied if it fills up).
///
/// Faithful port of the reference `pack_huffman()`.
pub fn pack_freetext(text: &str) -> ([u8; 72], usize) {
    let mut a = [0u8; 72];
    a[0] = 1;
    a[1] = 0;
    let mut bi = 2usize;
    let mut consumed = 0usize;

    // leave room for at least one 0 bit of padding: bi < 72-1
    for ch in text.chars() {
        if bi >= 72 - 1 {
            break;
        }
        let c = ch.to_ascii_uppercase();
        if let Some((_, code)) = HUFFMAN.iter().find(|(k, _)| *k == c) {
            if bi + code.len() > 72 - 1 {
                break;
            }
            for b in code.bytes() {
                a[bi] = (b == b'1') as u8;
                bi += 1;
            }
            consumed += 1;
        } else {
            // unencodable character: reference skips it (counts as consumed)
            consumed += 1;
        }
    }

    // pad: a single 0 then all 1s
    a[bi] = 0;
    bi += 1;
    while bi < 72 {
        a[bi] = 1;
        bi += 1;
    }

    (a, consumed)
}

/// Decode a free-text message from the 87 message bits (`a87`).
/// Faithful port of the reference `unpack_huffman()`.
pub fn unpack_freetext(a87: &[u8; 87]) -> String {
    // padding is a trailing 0 followed by 1s; find that last 0 in [0..72).
    let mut end = 71usize;
    while end > 0 && a87[end] != 0 {
        end -= 1;
    }

    let mut out = String::new();
    let mut i = 2usize;
    while i < end {
        let mut matched: Option<(char, usize)> = None;
        for (ch, code) in HUFFMAN.iter() {
            let bits = code.as_bytes();
            let mut ok = true;
            let mut k = 0usize;
            while k < bits.len() && i + k < 72 {
                let want = (bits[k] == b'1') as u8;
                if a87[i + k] != want {
                    ok = false;
                    break;
                }
                k += 1;
            }
            if ok && k == bits.len() {
                matched = Some((*ch, bits.len()));
                break;
            }
        }
        match matched {
            Some((ch, len)) => {
                out.push(ch);
                i += len;
            }
            None => {
                out.push('?');
                i += 1;
            }
        }
    }

    if a87[73] != 0 {
        out.push_str("<>"); // end of the "over"
    }
    out
}
