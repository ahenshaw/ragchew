//! LDPC(174,87) encode and sum-product decode for JS8.
//!
//! Bit/LLR convention (matches the WSJT-X / `fate` reference):
//! a log-likelihood ratio is `log(P(bit=0) / P(bit=1))`, so a **positive**
//! LLR means the bit is more likely **0**.

use crate::js8::tables::{COLORDER, GEN, MN, NM};

/// Encode 87 plain (message) bits into a 174-bit codeword.
///
/// Mirrors the reference `ldpc_encode()`: the first 87 codeword bits are
/// parity (each the XOR of the plain bits selected by a `GEN` row), the last
/// 87 are the systematic message bits, then `COLORDER` permutes into codeword
/// order.
pub fn encode(plain: &[u8; 87]) -> [u8; 174] {
    let mut cw = [0u8; 174];

    for i in 0..87 {
        let mut sum = 0u8;
        for j in 0..87 {
            sum ^= plain[j] & GEN[i][j];
        }
        cw[i] = sum & 1;
    }
    for i in 0..87 {
        cw[87 + i] = plain[i];
    }

    let mut codeword = [0u8; 174];
    for i in 0..174 {
        codeword[COLORDER[i]] = cw[i];
    }
    codeword
}

/// Number of the 87 parity checks that a 174-bit codeword satisfies (87 = all).
pub fn check(cw: &[u8; 174]) -> u32 {
    let mut score = 0;
    for j in 0..87 {
        let mut x = 0u8;
        for ii in 0..7 {
            let i1 = NM[j][ii];
            if i1 != 0 {
                x ^= cw[(i1 - 1) as usize];
            }
        }
        if x == 0 {
            score += 1;
        }
    }
    score
}

// tanh clamped to the FT8 reference's saturation point.
#[inline]
fn fast_tanh(x: f64) -> f64 {
    if x < -4.97 {
        -1.0
    } else if x > 4.97 {
        1.0
    } else {
        x.tanh()
    }
}

/// Result of a decode attempt.
pub struct Decoded {
    /// 174 bits in the reference `plain` order; the 87 message bits are `[87..174]`.
    pub plain: [u8; 174],
    /// Parity checks satisfied by the best codeword found (87 = fully corrected).
    pub score: u32,
}

impl Decoded {
    /// True when every parity check passed.
    pub fn ok(&self) -> bool {
        self.score == 87
    }
    /// The 87 systematic message bits (`a87`).
    pub fn message_bits(&self) -> [u8; 87] {
        let mut a = [0u8; 87];
        a.copy_from_slice(&self.plain[87..174]);
        a
    }
}

/// Sum-product (belief-propagation) decode of 174 per-bit LLRs.
///
/// Faithful port of the reference `ldpc_decode()`. Returns as soon as a
/// fully-consistent codeword is found, otherwise the best partial result.
pub fn decode(llr: &[f64; 174], iters: usize) -> Decoded {
    // m[check][bit]: bit->check messages; e[check][bit]: check->bit messages.
    let mut m = [[0.0f64; 174]; 87];
    let mut e = [[0.0f64; 174]; 87];

    for i in 0..174 {
        for j in 0..87 {
            m[j][i] = llr[i];
        }
    }

    let mut best_score = -1i32;
    let mut best_cw = [0u8; 174];

    for _iter in 0..iters {
        // check -> bit
        for j in 0..87 {
            for ii1 in 0..7 {
                let i1 = NM[j][ii1];
                if i1 == 0 {
                    continue;
                }
                let i1 = (i1 - 1) as usize;
                let mut a = 1.0f64;
                for ii2 in 0..7 {
                    let i2 = NM[j][ii2];
                    if i2 != 0 && (i2 - 1) as usize != i1 {
                        a *= fast_tanh(m[j][(i2 - 1) as usize] / 2.0);
                    }
                }
                e[j][i1] = ((1.0 + a) / (1.0 - a)).ln();
            }
        }

        // hard decision
        let mut cw = [0u8; 174];
        for i in 0..174 {
            let mut l = llr[i];
            for j in 0..3 {
                l += e[(MN[i][j] - 1) as usize][i];
            }
            cw[i] = (l <= 0.0) as u8;
        }

        let score = check(&cw);
        if score == 87 {
            let mut plain = [0u8; 174];
            for i in 0..174 {
                plain[i] = cw[COLORDER[i]];
            }
            return Decoded { plain, score };
        }
        if score as i32 > best_score {
            best_cw = cw;
            best_score = score as i32;
        }

        // bit -> check
        for i in 0..174 {
            for ji1 in 0..3 {
                let j1 = (MN[i][ji1] - 1) as usize;
                let mut l = llr[i];
                for ji2 in 0..3 {
                    if ji1 != ji2 {
                        let j2 = (MN[i][ji2] - 1) as usize;
                        l += e[j2][i];
                    }
                }
                m[j1][i] = l;
            }
        }
    }

    let mut plain = [0u8; 174];
    for i in 0..174 {
        plain[i] = best_cw[COLORDER[i]];
    }
    Decoded {
        plain,
        score: best_score.max(0) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message() -> [u8; 87] {
        // arbitrary-but-fixed 87-bit pattern
        let mut a = [0u8; 87];
        for i in 0..87 {
            a[i] = ((i * 7 + 3) % 5 == 0) as u8;
        }
        a
    }

    #[test]
    fn encode_produces_valid_codeword() {
        let cw = encode(&sample_message());
        assert_eq!(check(&cw), 87, "generator output must satisfy all parity checks");
    }

    #[test]
    fn decode_recovers_from_bit_errors() {
        let a87 = sample_message();
        let cw = encode(&a87);
        // strong LLRs matching the codeword, then flip the sign of a handful
        // of bits to simulate channel errors.
        let mut llr = [0.0f64; 174];
        for i in 0..174 {
            llr[i] = if cw[i] == 0 { 6.0 } else { -6.0 };
        }
        for &i in &[2usize, 17, 40, 61, 100, 150] {
            llr[i] = -llr[i];
        }
        let dec = decode(&llr, 30);
        assert!(dec.ok(), "should correct 6 errors, score {}", dec.score);
        assert_eq!(dec.message_bits(), a87);
    }
}
