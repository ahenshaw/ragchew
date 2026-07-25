//! CRC-12 as used by JS8 (WSJT-X FT8 polynomial `0xc06`, with the JS8 `^= 42`).

/// Generator polynomial x^12 + x^11 + x^10 + x^2 + x^1 (0x1c06), one bit per entry.
const DIV: [u8; 13] = [1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0];

/// Compute the JS8 12-bit CRC over `msg` (one bit per byte, MSB-first order),
/// returning 12 bits. Mirrors the reference `ft8_crc()` exactly, including the
/// JS8-specific final XOR with 42 (bits 6, 8, 10).
pub fn ft8_crc12(msg: &[u8]) -> [u8; 12] {
    let n = msg.len();
    let mut buf = vec![0u8; n + 12];
    buf[..n].copy_from_slice(msg);

    for i in 0..n {
        if buf[i] != 0 {
            for j in 0..13 {
                buf[i + j] ^= DIV[j];
            }
        }
    }

    let mut out = [0u8; 12];
    out.copy_from_slice(&buf[n..n + 12]);

    // JS8: crc ^= 42  (0b0000_0010_1010 -> indices 6, 8, 10 MSB-first)
    out[6] ^= 1;
    out[8] ^= 1;
    out[10] ^= 1;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_message_is_just_the_xor_constant() {
        // With an all-zero input the polynomial division yields 0, so the CRC
        // is exactly the JS8 constant 42 = 0b0000_0010_1010.
        let crc = ft8_crc12(&[0u8; 76]);
        let expected = [0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 0];
        assert_eq!(crc, expected);
    }

    #[test]
    fn is_deterministic_and_sensitive() {
        let mut a = [0u8; 76];
        a[3] = 1;
        a[40] = 1;
        let c1 = ft8_crc12(&a);
        assert_eq!(c1, ft8_crc12(&a));
        a[40] = 0;
        assert_ne!(c1, ft8_crc12(&a), "flipping a bit should change the CRC");
    }
}
