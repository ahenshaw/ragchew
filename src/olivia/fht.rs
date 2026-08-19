//! Fast Hadamard (Walsh) transform, the core of Olivia's forward error
//! correction.
//!
//! Both directions are the unnormalized butterfly pair from Pawel Jalocha's
//! reference (`pj_fht.h`); `FHT(IFHT(x)) == len * x`. The sign convention
//! matters — it decides which Walsh function a character maps to — so these are
//! transcribed exactly rather than replaced with a "standard" Hadamard
//! transform.

/// Forward transform, in place. `data.len()` must be a power of two.
pub fn fht<T>(data: &mut [T])
where
    T: Copy + std::ops::Add<Output = T> + std::ops::Sub<Output = T>,
{
    let len = data.len();
    let mut step = 1;
    while step < len {
        let mut ptr = 0;
        while ptr < len {
            for i in ptr..ptr + step {
                let a = data[i];
                let b = data[i + step];
                data[i] = b + a;
                data[i + step] = b - a;
            }
            ptr += 2 * step;
        }
        step *= 2;
    }
}

/// Inverse transform, in place. `data.len()` must be a power of two.
pub fn ifht<T>(data: &mut [T])
where
    T: Copy + std::ops::Add<Output = T> + std::ops::Sub<Output = T>,
{
    let len = data.len();
    let mut step = len / 2;
    while step > 0 {
        let mut ptr = 0;
        while ptr < len {
            for i in ptr..ptr + step {
                let a = data[i];
                let b = data[i + step];
                data[i] = a - b;
                data[i + step] = a + b;
            }
            ptr += 2 * step;
        }
        step /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FHT ∘ IFHT` scales by the transform length, as the reference's
    /// unnormalized pair does.
    #[test]
    fn round_trip_scales_by_len() {
        for len in [2usize, 4, 8, 64] {
            for pos in 0..len {
                let mut v = vec![0i32; len];
                v[pos] = 1;
                let orig = v.clone();
                ifht(&mut v);
                // an IFHT of a delta is a ±1 Walsh function
                assert!(v.iter().all(|&x| x == 1 || x == -1), "len {len} pos {pos}: {v:?}");
                fht(&mut v);
                for (a, b) in v.iter().zip(&orig) {
                    assert_eq!(*a, *b * len as i32);
                }
            }
        }
    }

    /// Distinct characters give orthogonal Walsh functions — that is what buys
    /// Olivia its coding gain.
    #[test]
    fn walsh_functions_are_orthogonal() {
        let len = 64usize;
        let walsh = |pos: usize| {
            let mut v = vec![0i32; len];
            v[pos] = 1;
            ifht(&mut v);
            v
        };
        for a in 0..len {
            for b in (a + 1)..len {
                let dot: i32 = walsh(a).iter().zip(walsh(b).iter()).map(|(x, y)| x * y).sum();
                assert_eq!(dot, 0, "walsh {a} and {b} are not orthogonal");
            }
        }
    }
}
