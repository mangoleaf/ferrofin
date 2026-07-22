//! Port of `ShuffleExtensions.cs` — in-place Fisher-Yates shuffle over a slice,
//! with an RNG-injectable variant for determinism.

use rand::Rng;

/// Shuffles the items in a slice in place using the thread-local RNG.
pub fn shuffle<T>(list: &mut [T]) {
    shuffle_with(list, &mut rand::thread_rng());
}

/// Shuffles the items in a slice in place using the supplied RNG.
///
/// This is a faithful port of the C# loop: it walks `n` from `len` down to `2`,
/// picks `k` in `0..n`, and swaps `list[k]` with `list[n-1]`.
pub fn shuffle_with<T, R: Rng + ?Sized>(list: &mut [T], rng: &mut R) {
    let mut n = list.len();
    while n > 1 {
        let k = rng.gen_range(0..n);
        n -= 1;
        list.swap(k, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    #[test]
    fn shuffle_valid_correct() {
        let mut original = [0u8; 1 << 6];
        rand::thread_rng().fill_bytes(&mut original);
        let mut shuffled = original;
        shuffle(&mut shuffled);

        assert_ne!(original, shuffled);
    }
}
