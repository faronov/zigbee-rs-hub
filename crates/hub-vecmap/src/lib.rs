//! [`VecMap`] and [`VecSet`]: a map and a set kept as a vector sorted by
//! key, with the `BTreeMap`/`BTreeSet` API the hub uses (lookups, inserts,
//! removals, entries, ranges, iteration in key order, serde as a map or a
//! sequence) and the same results.
//!
//! Each `BTreeMap<K, V>` instantiation costs 2–11 KB of firmware code (node
//! splits, merges, steals, cursors); a `VecMap<K, V>` a few hundred bytes
//! (a binary search and the vector's insert and remove). Inserts and
//! removals move the later entries: O(n), for the hub's maps of up to a few
//! hundred entries. [`sort`] has insertion sorts for the same reason.

#![no_std]

extern crate alloc;

use alloc::vec::{self, Vec};
use core::borrow::Borrow;
use core::fmt;
use core::iter::FusedIterator;
use core::ops::{Bound, Index, RangeBounds};
use core::slice;

/// A map kept as a vector sorted by key.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VecMap<K, V> {
    e: Vec<(K, V)>,
}

impl<K, V> Default for VecMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for VecMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The index range of the entries within `range` (as `BTreeMap::range`,
/// which panics on a start after the end).
fn bounds<K, T, R>(e: &[(K, impl Sized)], range: &R) -> (usize, usize)
where
    K: Borrow<T>,
    T: Ord + ?Sized,
    R: RangeBounds<T>,
{
    let lo = match range.start_bound() {
        Bound::Included(s) => e.partition_point(|x| x.0.borrow() < s),
        Bound::Excluded(s) => e.partition_point(|x| x.0.borrow() <= s),
        Bound::Unbounded => 0,
    };
    let hi = match range.end_bound() {
        Bound::Included(s) => e.partition_point(|x| x.0.borrow() <= s),
        Bound::Excluded(s) => e.partition_point(|x| x.0.borrow() < s),
        Bound::Unbounded => e.len(),
    };
    if let (Bound::Included(s) | Bound::Excluded(s), Bound::Included(t) | Bound::Excluded(t)) =
        (range.start_bound(), range.end_bound())
    {
        if s > t {
            panic!("range start is greater than range end in VecMap");
        }
        if s == t
            && matches!(
                (range.start_bound(), range.end_bound()),
                (Bound::Excluded(_), Bound::Excluded(_))
            )
        {
            panic!("range start and end are equal and excluded in VecMap");
        }
    }
    (lo, hi.max(lo))
}

impl<K, V> VecMap<K, V> {
    /// An empty map.
    pub const fn new() -> Self {
        Self { e: Vec::new() }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.e.len()
    }

    /// No entries.
    pub fn is_empty(&self) -> bool {
        self.e.is_empty()
    }

    /// Removes every entry.
    pub fn clear(&mut self) {
        self.e.clear();
    }

    /// The entries in key order.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter(self.e.iter())
    }

    /// The entries in key order, values mutable.
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut(self.e.iter_mut())
    }

    /// The keys in order.
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys(self.e.iter())
    }

    /// The values in key order.
    pub fn values(&self) -> Values<'_, K, V> {
        Values(self.e.iter())
    }

    /// The values in key order, mutable.
    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        ValuesMut(self.e.iter_mut())
    }

    /// The keys in order, by value.
    pub fn into_keys(self) -> IntoKeys<K, V> {
        IntoKeys(self.e.into_iter())
    }

    /// The values in key order, by value.
    pub fn into_values(self) -> IntoValues<K, V> {
        IntoValues(self.e.into_iter())
    }

    /// The entry with the smallest key.
    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        self.e.first().map(|(k, v)| (k, v))
    }

    /// The entry with the largest key.
    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        self.e.last().map(|(k, v)| (k, v))
    }

    /// Removes the entry with the smallest key.
    pub fn pop_first(&mut self) -> Option<(K, V)> {
        (!self.e.is_empty()).then(|| self.e.remove(0))
    }

    /// Removes the entry with the largest key.
    pub fn pop_last(&mut self) -> Option<(K, V)> {
        self.e.pop()
    }

    /// Keeps the entries `f` accepts (visited in key order).
    pub fn retain<F: FnMut(&K, &mut V) -> bool>(&mut self, mut f: F) {
        self.e.retain_mut(|(k, v)| f(k, v));
    }
}

impl<K: Ord, V> VecMap<K, V> {
    fn find<Q: Ord + ?Sized>(&self, k: &Q) -> Result<usize, usize>
    where
        K: Borrow<Q>,
    {
        self.e.binary_search_by(|e| e.0.borrow().cmp(k))
    }

    /// The value of `k`.
    pub fn get<Q: Ord + ?Sized>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| &self.e[i].1)
    }

    /// The value of `k`, mutable.
    pub fn get_mut<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| &mut self.e[i].1)
    }

    /// The key and value of `k`.
    pub fn get_key_value<Q: Ord + ?Sized>(&self, k: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| (&self.e[i].0, &self.e[i].1))
    }

    /// Whether `k` has a value.
    pub fn contains_key<Q: Ord + ?Sized>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.find(k).is_ok()
    }

    /// Sets the value of `k` (an existing entry keeps its key); the
    /// previous value, if any.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        match self.find(&k) {
            Ok(i) => Some(core::mem::replace(&mut self.e[i].1, v)),
            Err(i) => {
                self.e.insert(i, (k, v));
                None
            }
        }
    }

    /// Removes `k`; its value, if any.
    pub fn remove<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| self.e.remove(i).1)
    }

    /// Removes `k`; its key and value, if any.
    pub fn remove_entry<Q: Ord + ?Sized>(&mut self, k: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
    {
        self.find(k).ok().map(|i| self.e.remove(i))
    }

    /// The entry of `k`, for in-place changes.
    pub fn entry(&mut self, k: K) -> Entry<'_, K, V> {
        match self.find(&k) {
            Ok(i) => Entry::Occupied(OccupiedEntry { e: &mut self.e, i }),
            Err(i) => Entry::Vacant(VacantEntry {
                e: &mut self.e,
                i,
                key: k,
            }),
        }
    }

    /// The entries with keys in `range`, in key order.
    pub fn range<T, R>(&self, range: R) -> Iter<'_, K, V>
    where
        K: Borrow<T>,
        T: Ord + ?Sized,
        R: RangeBounds<T>,
    {
        let (lo, hi) = bounds(&self.e, &range);
        Iter(self.e[lo..hi].iter())
    }

    /// The entries with keys in `range`, in key order, values mutable.
    pub fn range_mut<T, R>(&mut self, range: R) -> IterMut<'_, K, V>
    where
        K: Borrow<T>,
        T: Ord + ?Sized,
        R: RangeBounds<T>,
    {
        let (lo, hi) = bounds(&self.e, &range);
        IterMut(self.e[lo..hi].iter_mut())
    }

    /// Moves every entry of `other` into this map (`other`'s value wins
    /// for a key in both).
    pub fn append(&mut self, other: &mut Self) {
        for (k, v) in other.e.drain(..) {
            self.insert(k, v);
        }
    }

    /// Splits off the entries from `k` on.
    pub fn split_off<Q: Ord + ?Sized>(&mut self, k: &Q) -> Self
    where
        K: Borrow<Q>,
    {
        let i = self.e.partition_point(|e| e.0.borrow() < k);
        Self {
            e: self.e.split_off(i),
        }
    }
}

impl<K: Ord + Borrow<Q>, Q: Ord + ?Sized, V> Index<&Q> for VecMap<K, V> {
    type Output = V;

    fn index(&self, k: &Q) -> &V {
        self.get(k).expect("no entry found for key")
    }
}

/// Later entries replace earlier ones with the same key (as collecting
/// into a `BTreeMap`).
impl<K: Ord, V> FromIterator<(K, V)> for VecMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(it: I) -> Self {
        let mut m = Self::new();
        m.extend(it);
        m
    }
}

impl<K: Ord, V> Extend<(K, V)> for VecMap<K, V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, it: I) {
        for (k, v) in it {
            self.insert(k, v);
        }
    }
}

impl<'a, K: Ord + Copy, V: Copy> Extend<(&'a K, &'a V)> for VecMap<K, V> {
    fn extend<I: IntoIterator<Item = (&'a K, &'a V)>>(&mut self, it: I) {
        for (k, v) in it {
            self.insert(*k, *v);
        }
    }
}

impl<K: Ord, V, const N: usize> From<[(K, V); N]> for VecMap<K, V> {
    fn from(a: [(K, V); N]) -> Self {
        a.into_iter().collect()
    }
}

impl<K, V> IntoIterator for VecMap<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;

    fn into_iter(self) -> IntoIter<K, V> {
        IntoIter(self.e.into_iter())
    }
}

impl<'a, K, V> IntoIterator for &'a VecMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K, V> IntoIterator for &'a mut VecMap<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

/// An entry of a [`VecMap`] ([`VecMap::entry`]).
pub enum Entry<'a, K, V> {
    /// No value for the key yet.
    Vacant(VacantEntry<'a, K, V>),
    /// The key has a value.
    Occupied(OccupiedEntry<'a, K, V>),
}

/// A key without a value ([`Entry::Vacant`]).
pub struct VacantEntry<'a, K, V> {
    e: &'a mut Vec<(K, V)>,
    i: usize,
    key: K,
}

/// A key with its value ([`Entry::Occupied`]).
pub struct OccupiedEntry<'a, K, V> {
    e: &'a mut Vec<(K, V)>,
    i: usize,
}

impl<'a, K, V> Entry<'a, K, V> {
    /// The value, `v` inserted when missing.
    pub fn or_insert(self, v: V) -> &'a mut V {
        match self {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(e) => e.insert(v),
        }
    }

    /// The value, `f()` inserted when missing.
    pub fn or_insert_with<F: FnOnce() -> V>(self, f: F) -> &'a mut V {
        match self {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(e) => e.insert(f()),
        }
    }

    /// The value, `f(key)` inserted when missing.
    pub fn or_insert_with_key<F: FnOnce(&K) -> V>(self, f: F) -> &'a mut V {
        match self {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(e) => {
                let v = f(&e.key);
                e.insert(v)
            }
        }
    }

    /// The value, a default one inserted when missing.
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }

    /// Applies `f` to an existing value.
    pub fn and_modify<F: FnOnce(&mut V)>(self, f: F) -> Self {
        match self {
            Entry::Occupied(mut o) => {
                f(o.get_mut());
                Entry::Occupied(o)
            }
            e => e,
        }
    }

    /// The entry's key.
    pub fn key(&self) -> &K {
        match self {
            Entry::Occupied(o) => o.key(),
            Entry::Vacant(e) => e.key(),
        }
    }
}

impl<'a, K, V> VacantEntry<'a, K, V> {
    /// The key.
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The key, by value.
    pub fn into_key(self) -> K {
        self.key
    }

    /// Inserts `v` for the key; the value in the map.
    pub fn insert(self, v: V) -> &'a mut V {
        self.e.insert(self.i, (self.key, v));
        &mut self.e[self.i].1
    }
}

impl<'a, K, V> OccupiedEntry<'a, K, V> {
    /// The key.
    pub fn key(&self) -> &K {
        &self.e[self.i].0
    }

    /// The value.
    pub fn get(&self) -> &V {
        &self.e[self.i].1
    }

    /// The value, mutable.
    pub fn get_mut(&mut self) -> &mut V {
        &mut self.e[self.i].1
    }

    /// The value in the map.
    pub fn into_mut(self) -> &'a mut V {
        &mut self.e[self.i].1
    }

    /// Replaces the value; the previous one.
    pub fn insert(&mut self, v: V) -> V {
        core::mem::replace(&mut self.e[self.i].1, v)
    }

    /// Removes the entry; its value.
    pub fn remove(self) -> V {
        self.e.remove(self.i).1
    }

    /// Removes the entry; its key and value.
    pub fn remove_entry(self) -> (K, V) {
        self.e.remove(self.i)
    }
}

/// Iterator plumbing: `$name` wraps `$inner`, maps each item with `$f`.
macro_rules! iter {
    ($(#[$m:meta])* $name:ident<$($lt:lifetime,)? $($g:ident),*>, $inner:ty, $item:ty, $f:expr) => {
        $(#[$m])*
        pub struct $name<$($lt,)? $($g),*>($inner);

        impl<$($lt,)? $($g),*> Iterator for $name<$($lt,)? $($g),*> {
            type Item = $item;

            fn next(&mut self) -> Option<$item> {
                self.0.next().map($f)
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                self.0.size_hint()
            }
        }

        impl<$($lt,)? $($g),*> DoubleEndedIterator for $name<$($lt,)? $($g),*> {
            fn next_back(&mut self) -> Option<$item> {
                self.0.next_back().map($f)
            }
        }

        impl<$($lt,)? $($g),*> ExactSizeIterator for $name<$($lt,)? $($g),*> {}
        impl<$($lt,)? $($g),*> FusedIterator for $name<$($lt,)? $($g),*> {}
    };
}

iter!(
    /// [`VecMap::iter`], [`VecMap::range`].
    Iter<'a, K, V>, slice::Iter<'a, (K, V)>, (&'a K, &'a V), |(k, v): &'a (K, V)| (k, v)
);
iter!(
    /// [`VecMap::iter_mut`], [`VecMap::range_mut`].
    IterMut<'a, K, V>, slice::IterMut<'a, (K, V)>, (&'a K, &'a mut V), |(k, v): &'a mut (K, V)| (&*k, v)
);
iter!(
    /// [`VecMap::keys`].
    Keys<'a, K, V>, slice::Iter<'a, (K, V)>, &'a K, |e: &'a (K, V)| &e.0
);
iter!(
    /// [`VecMap::values`].
    Values<'a, K, V>, slice::Iter<'a, (K, V)>, &'a V, |e: &'a (K, V)| &e.1
);
iter!(
    /// [`VecMap::values_mut`].
    ValuesMut<'a, K, V>, slice::IterMut<'a, (K, V)>, &'a mut V, |e: &'a mut (K, V)| &mut e.1
);
iter!(
    /// [`VecMap::into_iter`].
    IntoIter<K, V>, vec::IntoIter<(K, V)>, (K, V), |e| e
);
iter!(
    /// [`VecMap::into_keys`].
    IntoKeys<K, V>, vec::IntoIter<(K, V)>, K, |e: (K, V)| e.0
);
iter!(
    /// [`VecMap::into_values`].
    IntoValues<K, V>, vec::IntoIter<(K, V)>, V, |e: (K, V)| e.1
);
iter!(
    /// [`VecSet::iter`], [`VecSet::range`].
    SetIter<'a, T>, slice::Iter<'a, (T, ())>, &'a T, |e: &'a (T, ())| &e.0
);
iter!(
    /// [`VecSet::into_iter`].
    SetIntoIter<T>, vec::IntoIter<(T, ())>, T, |e: (T, ())| e.0
);

impl<K, V> Clone for Iter<'_, K, V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<K, V> Clone for Keys<'_, K, V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<K, V> Clone for Values<'_, K, V> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Clone for SetIter<'_, T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// A set kept as a vector sorted by value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VecSet<T> {
    m: VecMap<T, ()>,
}

impl<T> Default for VecSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for VecSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<T> VecSet<T> {
    /// An empty set.
    pub const fn new() -> Self {
        Self { m: VecMap::new() }
    }

    /// Number of values.
    pub fn len(&self) -> usize {
        self.m.len()
    }

    /// No values.
    pub fn is_empty(&self) -> bool {
        self.m.is_empty()
    }

    /// Removes every value.
    pub fn clear(&mut self) {
        self.m.clear();
    }

    /// The values in order.
    pub fn iter(&self) -> SetIter<'_, T> {
        SetIter(self.m.e.iter())
    }

    /// The smallest value.
    pub fn first(&self) -> Option<&T> {
        self.m.e.first().map(|e| &e.0)
    }

    /// The largest value.
    pub fn last(&self) -> Option<&T> {
        self.m.e.last().map(|e| &e.0)
    }

    /// Removes the smallest value.
    pub fn pop_first(&mut self) -> Option<T> {
        self.m.pop_first().map(|e| e.0)
    }

    /// Removes the largest value.
    pub fn pop_last(&mut self) -> Option<T> {
        self.m.pop_last().map(|e| e.0)
    }

    /// Keeps the values `f` accepts (visited in order).
    pub fn retain<F: FnMut(&T) -> bool>(&mut self, mut f: F) {
        self.m.e.retain(|e| f(&e.0));
    }
}

impl<T: Ord> VecSet<T> {
    /// Adds `v`; false when it was there already (the set keeps the old
    /// value).
    pub fn insert(&mut self, v: T) -> bool {
        match self.m.find(&v) {
            Ok(_) => false,
            Err(i) => {
                self.m.e.insert(i, (v, ()));
                true
            }
        }
    }

    /// Adds `v`, replacing an equal value; the replaced one.
    pub fn replace(&mut self, v: T) -> Option<T> {
        match self.m.find(&v) {
            Ok(i) => Some(core::mem::replace(&mut self.m.e[i].0, v)),
            Err(i) => {
                self.m.e.insert(i, (v, ()));
                None
            }
        }
    }

    /// Whether the set has `v`.
    pub fn contains<Q: Ord + ?Sized>(&self, v: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.m.contains_key(v)
    }

    /// The set's value equal to `v`.
    pub fn get<Q: Ord + ?Sized>(&self, v: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
    {
        self.m.get_key_value(v).map(|(k, _)| k)
    }

    /// Removes `v`; whether it was there.
    pub fn remove<Q: Ord + ?Sized>(&mut self, v: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.m.remove(v).is_some()
    }

    /// Removes and returns the value equal to `v`.
    pub fn take<Q: Ord + ?Sized>(&mut self, v: &Q) -> Option<T>
    where
        T: Borrow<Q>,
    {
        self.m.remove_entry(v).map(|e| e.0)
    }

    /// The values in `range`, in order.
    pub fn range<Q, R>(&self, range: R) -> SetIter<'_, T>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
        R: RangeBounds<Q>,
    {
        let (lo, hi) = bounds(&self.m.e, &range);
        SetIter(self.m.e[lo..hi].iter())
    }

    /// Moves every value of `other` into this set.
    pub fn append(&mut self, other: &mut Self) {
        for (v, ()) in other.m.e.drain(..) {
            self.insert(v);
        }
    }

    /// The values of this set not in `other`, in order.
    pub fn difference<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = &'a T> + 'a {
        self.iter().filter(move |v| !other.contains(*v))
    }

    /// The values in both sets, in order.
    pub fn intersection<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = &'a T> + 'a {
        self.iter().filter(move |v| other.contains(*v))
    }

    /// Whether every value of this set is in `other`.
    pub fn is_subset(&self, other: &Self) -> bool {
        self.len() <= other.len() && self.iter().all(|v| other.contains(v))
    }

    /// Whether every value of `other` is in this set.
    pub fn is_superset(&self, other: &Self) -> bool {
        other.is_subset(self)
    }

    /// Whether the sets have no value in common.
    pub fn is_disjoint(&self, other: &Self) -> bool {
        self.intersection(other).next().is_none()
    }
}

impl<T: Ord> FromIterator<T> for VecSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(it: I) -> Self {
        let mut s = Self::new();
        s.extend(it);
        s
    }
}

impl<T: Ord> Extend<T> for VecSet<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, it: I) {
        for v in it {
            self.insert(v);
        }
    }
}

impl<'a, T: Ord + Copy + 'a> Extend<&'a T> for VecSet<T> {
    fn extend<I: IntoIterator<Item = &'a T>>(&mut self, it: I) {
        for v in it {
            self.insert(*v);
        }
    }
}

impl<T: Ord, const N: usize> From<[T; N]> for VecSet<T> {
    fn from(a: [T; N]) -> Self {
        a.into_iter().collect()
    }
}

impl<T> IntoIterator for VecSet<T> {
    type Item = T;
    type IntoIter = SetIntoIter<T>;

    fn into_iter(self) -> SetIntoIter<T> {
        SetIntoIter(self.m.e.into_iter())
    }
}

impl<'a, T> IntoIterator for &'a VecSet<T> {
    type Item = &'a T;
    type IntoIter = SetIter<'a, T>;

    fn into_iter(self) -> SetIter<'a, T> {
        self.iter()
    }
}

/// Stable insertion sorts for short slices: a few hundred bytes of
/// firmware code per element type where `slice::sort*` instantiates core's
/// driftsort or ipnsort (2–5 KB each on Xtensa). The order of
/// `slice::sort_by` (stable; for an unstable sort's callers, the same
/// result whenever equal elements are indistinguishable); quadratic, for
/// slices of a few hundred elements at most.
pub mod sort {
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

    /// Sorts `v` stably.
    pub fn sort<T: Ord>(v: &mut [T]) {
        sort_by(v, |a, b| a < b);
    }
}

#[cfg(feature = "serde")]
mod serde_impls {
    use core::fmt;
    use core::marker::PhantomData;

    use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    use serde::ser::{Serialize, Serializer};

    use super::{VecMap, VecSet};

    impl<K: Serialize, V: Serialize> Serialize for VecMap<K, V> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.collect_map(self.iter())
        }
    }

    impl<T: Serialize> Serialize for VecSet<T> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(self.iter())
        }
    }

    struct MapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> Visitor<'de> for MapVisitor<K, V> {
        type Value = VecMap<K, V>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut m = VecMap::new();
            while let Some((k, v)) = a.next_entry()? {
                m.insert(k, v);
            }
            Ok(m)
        }
    }

    impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> Deserialize<'de> for VecMap<K, V> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_map(MapVisitor(PhantomData))
        }
    }

    struct SetVisitor<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de> + Ord> Visitor<'de> for SetVisitor<T> {
        type Value = VecSet<T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a sequence")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut s = VecSet::new();
            while let Some(v) = a.next_element()? {
                s.insert(v);
            }
            Ok(s)
        }
    }

    impl<'de, T: Deserialize<'de> + Ord> Deserialize<'de> for VecSet<T> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_seq(SetVisitor(PhantomData))
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::collections::{BTreeMap, BTreeSet};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::format;

    use super::{Entry, VecMap, VecSet};

    /// Every operation on a `VecMap` and a `BTreeMap` side by side.
    #[test]
    fn map_matches_btreemap() {
        let mut v: VecMap<u16, String> = VecMap::new();
        let mut b: BTreeMap<u16, String> = BTreeMap::new();
        let mut x: u32 = 7;
        for step in 0..2000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let k = ((x >> 8) % 64) as u16;
            let s = step.to_string();
            match x % 7 {
                0 | 1 => assert_eq!(v.insert(k, s.clone()), b.insert(k, s)),
                2 => assert_eq!(v.remove(&k), b.remove(&k)),
                3 => {
                    let a = v.entry(k).or_insert_with(|| s.clone()).clone();
                    assert_eq!(a, *b.entry(k).or_insert(s));
                }
                4 => {
                    v.entry(k).and_modify(|e| e.push('m'));
                    b.entry(k).and_modify(|e| e.push('m'));
                }
                5 => {
                    let lo = k.saturating_sub(5);
                    assert!(v.range(lo..k).eq(b.range(lo..k)));
                    assert!(v.range(lo..=k).rev().eq(b.range(lo..=k).rev()));
                    assert!(v.range(k..).eq(b.range(k..)));
                }
                _ => {
                    v.retain(|k, _| k % 5 != 0);
                    b.retain(|k, _| k % 5 != 0);
                }
            }
            assert_eq!(v.get(&k), b.get(&k));
            assert_eq!(v.len(), b.len());
        }
        assert!(v.iter().eq(b.iter()));
        assert!(v.keys().eq(b.keys()) && v.values().eq(b.values()));
        assert_eq!(v.first_key_value(), b.first_key_value());
        assert_eq!(v.last_key_value(), b.last_key_value());
        assert_eq!(format!("{v:?}"), format!("{b:?}"));
        assert_eq!(v.pop_first(), b.pop_first());
        assert_eq!(v.pop_last(), b.pop_last());
        assert!(v.clone().into_iter().eq(b.clone()));
    }

    #[test]
    fn entries_and_collecting() {
        let mut m: VecMap<&str, i32> = [("b", 1), ("a", 2), ("b", 3)].into();
        assert!(m.iter().eq([(&"a", &2), (&"b", &3)]));
        match m.entry("a") {
            Entry::Occupied(mut o) => assert_eq!(o.insert(5), 2),
            Entry::Vacant(_) => unreachable!(),
        }
        match m.entry("c") {
            Entry::Vacant(e) => *e.insert(1) += 1,
            Entry::Occupied(_) => unreachable!(),
        }
        *m.entry("d").or_default() += 4;
        assert_eq!(m["c"], 2);
        assert!(
            m.iter()
                .eq([(&"a", &5), (&"b", &3), (&"c", &2), (&"d", &4)])
        );
        let tail = m.split_off("c");
        assert!(m.keys().eq(&["a", "b"]) && tail.keys().eq(&["c", "d"]));
        for (_, v) in &mut m {
            *v *= 10;
        }
        assert!(m.into_values().eq([50, 30]));
    }

    #[test]
    fn set_matches_btreeset() {
        let mut v: VecSet<u8> = [5, 1, 5, 3].into_iter().collect();
        let mut b: BTreeSet<u8> = [5, 1, 5, 3].into_iter().collect();
        assert_eq!(v.insert(2), b.insert(2));
        assert_eq!(v.insert(2), b.insert(2));
        assert_eq!(v.remove(&1), b.remove(&1));
        assert!(v.contains(&3) && !v.contains(&1));
        assert!(v.iter().eq(b.iter()) && v.range(3..).eq(b.range(3..)));
        assert_eq!(format!("{v:?}"), format!("{b:?}"));
        let w: VecSet<u8> = [3, 9].into();
        assert!(v.difference(&w).eq([2, 5].iter()));
        assert!(v.intersection(&w).eq([3].iter()));
        assert!(!v.is_subset(&w) && VecSet::from([3]).is_subset(&w));
        assert!(v.into_iter().eq(b));
    }

    #[test]
    fn sorts_like_slice_sort() {
        let mut x = 0x1234_5678u32;
        for len in [0usize, 1, 2, 3, 20, 200] {
            // Many ties; the index tells equal keys apart (stability).
            let v: Vec<(u8, usize)> = (0..len)
                .map(|i| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    ((x % 7) as u8, i)
                })
                .collect();
            let (mut a, mut b) = (v.clone(), v.clone());
            super::sort::sort_by_key(&mut a, |e| core::cmp::Reverse(e.0));
            b.sort_by_key(|e| core::cmp::Reverse(e.0));
            assert_eq!(a, b);
            let (mut a, mut b) = (v.clone(), v);
            super::sort::sort(&mut a);
            b.sort_unstable();
            assert_eq!(a, b);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_as_btree() {
        let b: BTreeMap<u64, Vec<u8>> = [(10, Vec::from([1u8])), (2, Vec::new())].into();
        let v: VecMap<u64, Vec<u8>> = b.clone().into_iter().collect();
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<VecMap<u64, Vec<u8>>>(&json).unwrap(),
            v
        );
        // Duplicate keys: the last value wins, as for a BTreeMap.
        let dup = r#"{"a":1,"b":2,"a":3}"#;
        let vb: BTreeMap<String, u8> = serde_json::from_str(dup).unwrap();
        let vv: VecMap<String, u8> = serde_json::from_str(dup).unwrap();
        assert!(vv.iter().eq(vb.iter()));
        let s: VecSet<String> = serde_json::from_str(r#"["b","a","b"]"#).unwrap();
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"["a","b"]"#);
    }
}
