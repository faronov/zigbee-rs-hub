//! Maps that cost less firmware code: [`VecMap`] and [`VecSet`] (crate
//! `hub-vecmap`), kept as vectors sorted by key with the `BTreeMap` and
//! `BTreeSet` API, in a fraction of the code of a B-tree instantiation,
//! and [`btree_map`]/[`btree_set`], which build a `BTreeMap`/`BTreeSet`
//! without the sort that collecting into one instantiates.

use std::collections::{BTreeMap, BTreeSet};

pub use hub_vecmap::{VecMap, VecSet};

/// The `BTreeMap` of `it`, as `collect()` builds it (a later entry
/// replaces an earlier one with the same key), by inserts: `collect()`
/// sorts the entries first, with a sort instantiated per iterator type
/// (~3 KB of firmware each).
pub fn btree_map<K: Ord, V>(it: impl IntoIterator<Item = (K, V)>) -> BTreeMap<K, V> {
    let mut m = BTreeMap::new();
    for (k, v) in it {
        m.insert(k, v);
    }
    m
}

/// The `BTreeSet` of `it`, as `collect()` builds it, by inserts:
/// `collect()` sorts first, with a sort instantiated per element type.
pub fn btree_set<T: Ord>(it: impl IntoIterator<Item = T>) -> BTreeSet<T> {
    let mut s = BTreeSet::new();
    for x in it {
        s.insert(x);
    }
    s
}
