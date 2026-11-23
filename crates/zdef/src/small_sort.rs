//! Stable sorts for the short slices the encoders order (map keys,
//! endpoints, entities): an insertion sort, a few hundred bytes per element
//! type where `slice::sort_by` instantiates core's driftsort (~3 KB each on
//! Xtensa). Same order as `slice::sort_by` (stable); quadratic, so only for
//! slices of a few hundred elements at most.

/// Sorts `v` stably by `less` (the "is less than" relation).
pub fn sort_by<T>(v: &mut [T], mut less: impl FnMut(&T, &T) -> bool) {
    for i in 1..v.len() {
        let mut j = i;
        while j > 0 && less(&v[j], &v[j - 1]) {
            v.swap(j, j - 1);
            j -= 1;
        }
    }
}

/// Sorts `v` stably by the key `f` returns.
pub fn sort_by_key<T, K: Ord>(v: &mut [T], mut f: impl FnMut(&T) -> K) {
    sort_by(v, |a, b| f(a) < f(b));
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    #[test]
    fn same_order_as_the_std_stable_sort() {
        // Pseudo-random keys with many ties; the second field tells ties apart.
        let mut x = 0x1234_5678u32;
        for len in [0usize, 1, 2, 3, 7, 20, 21, 64, 200] {
            let v: Vec<(u8, usize)> = (0..len)
                .map(|i| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    ((x % 7) as u8, i)
                })
                .collect();
            let mut ours = v.clone();
            super::sort_by_key(&mut ours, |e| e.0);
            let mut std = v.clone();
            std.sort_by_key(|e| e.0);
            assert_eq!(ours, std, "len {len}");
            let mut ours = v.clone();
            super::sort_by(&mut ours, |a, b| a.0 > b.0);
            let mut std = v;
            std.sort_by_key(|e| core::cmp::Reverse(e.0));
            assert_eq!(ours, std, "len {len}, descending");
        }
    }
}
