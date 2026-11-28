//! Maps that cost less firmware code: [`VecMap`], a map kept as a vector
//! sorted by key, for the small maps of the reporting configuration
//! (lookups, inserts, removals and iteration in key order like a
//! `BTreeMap`, in a fraction of the code of a `BTreeMap` instantiation),
//! and [`btree_map`]/[`btree_set`], which build a `BTreeMap`/`BTreeSet`
//! without the sort that collecting into one instantiates.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

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

/// A map kept as a vector sorted by key (iterates in key order).
#[derive(Debug, Clone, PartialEq)]
pub struct VecMap<K, V>(Vec<(K, V)>);

impl<K, V> Default for VecMap<K, V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<K: Ord, V> VecMap<K, V> {
    /// An empty map.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    fn find<Q: Ord + ?Sized>(&self, k: &Q) -> Result<usize, usize>
    where
        K: Borrow<Q>,
    {
        self.0.binary_search_by(|e| e.0.borrow().cmp(k))
    }

    /// The value of `k`.
    pub fn get<Q: Ord + ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| &self.0[i].1)
    }

    /// The value of `k`, mutable.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.find(k).ok().map(|i| &mut self.0[i].1)
    }

    /// [`Self::get_mut`] by a borrowed key.
    pub fn get_mut_by<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| &mut self.0[i].1)
    }

    /// The value of `k`, a default one inserted when missing.
    pub fn entry_default<Q: Ord + ?Sized + ToOwned<Owned = K>>(&mut self, k: &Q) -> &mut V
    where
        K: Borrow<Q>,
        V: Default,
    {
        let i = match self.find(k) {
            Ok(i) => i,
            Err(i) => {
                self.0.insert(i, (k.to_owned(), V::default()));
                i
            }
        };
        &mut self.0[i].1
    }

    /// Whether `k` has a value.
    pub fn contains_key<Q: Ord + ?Sized>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.find(k).is_ok()
    }

    /// Sets the value of `k`; the previous one, if any.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.find(&k) {
            Ok(i) => Some(std::mem::replace(&mut self.0[i].1, v)),
            Err(i) => {
                self.0.insert(i, (k, v));
                None
            }
        }
    }

    /// Removes `k`; its value, if any.
    pub fn remove<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| self.0.remove(i).1)
    }
}

impl<K, V> VecMap<K, V> {
    /// No entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The entries in key order.
    pub fn iter(&self) -> std::slice::Iter<'_, (K, V)> {
        self.0.iter()
    }
}

/// Later entries replace earlier ones with the same key (as collecting
/// into a `BTreeMap`).
impl<K: Ord, V> FromIterator<(K, V)> for VecMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(it: I) -> Self {
        let mut m = Self::new();
        for (k, v) in it {
            m.insert(k, v);
        }
        m
    }
}

impl<'a, K, V> IntoIterator for &'a VecMap<K, V> {
    type Item = &'a (K, V);
    type IntoIter = std::slice::Iter<'a, (K, V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::VecMap;

    #[test]
    fn behaves_like_a_btreemap() {
        let mut m: VecMap<u16, &str> = [(5, "a"), (1, "b"), (5, "c"), (3, "d")]
            .into_iter()
            .collect();
        assert_eq!(
            m.iter().copied().collect::<Vec<_>>(),
            [(1, "b"), (3, "d"), (5, "c")]
        );
        assert_eq!(m.insert(3, "e"), Some("d"));
        assert_eq!(m.insert(2, "f"), None);
        assert_eq!(m.remove(&1), Some("b"));
        assert_eq!(m.remove(&1), None);
        assert_eq!((m.get(&2), m.get(&4)), (Some(&"f"), None));
        assert!(m.contains_key(&5) && !m.contains_key(&1));
        *m.get_mut(&5).unwrap() = "g";
        assert_eq!(
            m.iter().copied().collect::<Vec<_>>(),
            [(2, "f"), (3, "e"), (5, "g")]
        );
        assert_eq!(m.len(), 3);
    }
}
