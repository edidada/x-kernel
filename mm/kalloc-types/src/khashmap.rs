// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible hash map ([`KHashMap`]) backed by `hashbrown::HashMap`.
//!
//! Wraps the underlying map and provides `try_*` entry points for every
//! operation that may grow the backing table. Allocation failure returns
//! [`AllocError`] instead of panicking.

use core::{borrow::Borrow, fmt, hash::Hash};

use hashbrown::hash_map::{self, HashMap};

use crate::{AllocContext, AllocError, context::require_supported_context};

/// A fallible hash map, analogous to `hashbrown::HashMap`.
///
/// All operations that may allocate (insertion, reservation) take an
/// explicit [`AllocContext`] and return `Result`. Operations that never
/// allocate (lookup, removal, iteration) delegate directly to the inner map.
pub struct KHashMap<K, V, S = hashbrown::DefaultHashBuilder> {
    inner: HashMap<K, V, S>,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl<K, V> KHashMap<K, V> {
    /// Creates an empty map without allocating.
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, V> KHashMap<K, V> {
    /// Creates an empty map with at least the given capacity.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the pre-allocation.
    pub fn try_with_capacity(capacity: usize, context: AllocContext) -> Result<Self, AllocError> {
        require_supported_context(context)?;
        let mut inner = HashMap::new();
        inner
            .try_reserve(capacity)
            .map_err(|_| AllocError::NoMemory)?;
        Ok(Self { inner })
    }
}

impl<K, V, S> KHashMap<K, V, S> {
    /// Returns the number of elements in the map.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the map is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of elements the map can hold without reallocating.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}

// ---------------------------------------------------------------------------
// Fallible operations
// ---------------------------------------------------------------------------

impl<K, V, S> KHashMap<K, V, S>
where
    K: Eq + Hash,
    S: core::hash::BuildHasher,
{
    /// Reserves capacity for at least `additional` more elements.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the growth.
    pub fn try_reserve(
        &mut self,
        additional: usize,
        context: AllocContext,
    ) -> Result<(), AllocError> {
        require_supported_context(context)?;
        self.inner
            .try_reserve(additional)
            .map_err(|_| AllocError::NoMemory)
    }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not have this key present, `None` is returned. If the
    /// map did have this key, the value is updated and the old value is
    /// returned.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the table must grow and the
    /// kernel allocator cannot satisfy it.
    pub fn try_insert(
        &mut self,
        k: K,
        v: V,
        context: AllocContext,
    ) -> Result<Option<V>, AllocError> {
        require_supported_context(context)?;
        // try_reserve is cheap when the table already has room.
        self.inner
            .try_reserve(1)
            .map_err(|_| AllocError::NoMemory)?;
        Ok(self.inner.insert(k, v))
    }

    /// Removes a key from the map, returning the value at the key if it was
    /// present. Does not allocate.
    pub fn remove<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.remove(k)
    }

    /// Returns a reference to the value corresponding to the key.
    pub fn get<Q>(&self, k: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get(k)
    }

    /// Returns a mutable reference to the value corresponding to the key.
    pub fn get_mut<Q>(&mut self, k: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get_mut(k)
    }

    /// Returns `true` if the map contains a value for the specified key.
    pub fn contains_key<Q>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.contains_key(k)
    }

    /// An iterator visiting all key-value pairs in arbitrary order.
    pub fn iter(&self) -> hash_map::Iter<'_, K, V> {
        self.inner.iter()
    }

    /// An iterator visiting all keys in arbitrary order.
    pub fn keys(&self) -> hash_map::Keys<'_, K, V> {
        self.inner.keys()
    }

    /// An iterator visiting all values in arbitrary order.
    pub fn values(&self) -> hash_map::Values<'_, K, V> {
        self.inner.values()
    }

    /// Removes all entries. Does not deallocate the backing table.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

impl<K, V, S: Default> Default for KHashMap<K, V, S> {
    fn default() -> Self {
        Self {
            inner: HashMap::default(),
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S> fmt::Debug for KHashMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<K, V, S> IntoIterator for KHashMap<K, V, S> {
    type IntoIter = hash_map::IntoIter<K, V>;
    type Item = (K, V);

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

impl<'a, K, V, S> IntoIterator for &'a KHashMap<K, V, S> {
    type IntoIter = hash_map::Iter<'a, K, V>;
    type Item = (&'a K, &'a V);

    fn into_iter(self) -> Self::IntoIter {
        self.inner.iter()
    }
}

// SAFETY: KHashMap wraps hashbrown::HashMap which is Send when K,V,S are Send.
unsafe impl<K: Send, V: Send, S: Send> Send for KHashMap<K, V, S> {}

// SAFETY: KHashMap wraps hashbrown::HashMap which is Sync when K,V,S are Sync.
unsafe impl<K: Sync, V: Sync, S: Sync> Sync for KHashMap<K, V, S> {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn new_is_empty() {
        let m = KHashMap::<u32, u32>::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[def_test]
    fn try_with_capacity_preallocates() {
        let m = KHashMap::<u32, u32>::try_with_capacity(16, AllocContext::Sleepable).unwrap();
        assert!(m.is_empty());
        assert!(m.capacity() >= 16);
    }

    #[def_test]
    fn try_insert_new_key() {
        let mut m = KHashMap::new();
        let old = m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        assert!(old.is_none());
        assert_eq!(m.len(), 1);
    }

    #[def_test]
    fn try_insert_replace_existing() {
        let mut m = KHashMap::new();
        m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        let old = m.try_insert(1, "b", AllocContext::Sleepable).unwrap();
        assert_eq!(old, Some("a"));
        assert_eq!(m.get(&1), Some(&"b"));
        assert_eq!(m.len(), 1);
    }

    #[def_test]
    fn get_existing_and_missing() {
        let mut m = KHashMap::new();
        m.try_insert(42, "x", AllocContext::Sleepable).unwrap();
        assert_eq!(m.get(&42), Some(&"x"));
        assert_eq!(m.get(&99), None);
    }

    #[def_test]
    fn remove_existing() {
        let mut m = KHashMap::new();
        m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        assert_eq!(m.remove(&1), Some("a"));
        assert!(m.is_empty());
    }

    #[def_test]
    fn remove_missing() {
        let mut m = KHashMap::new();
        m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        assert_eq!(m.remove(&2), None);
    }

    #[def_test]
    fn contains_key() {
        let mut m = KHashMap::new();
        m.try_insert("key", 42, AllocContext::Sleepable).unwrap();
        assert!(m.contains_key("key"));
        assert!(!m.contains_key("nope"));
    }

    #[def_test]
    fn get_mut_modify_in_place() {
        let mut m = KHashMap::new();
        m.try_insert(1, 10, AllocContext::Sleepable).unwrap();
        *m.get_mut(&1).unwrap() += 5;
        assert_eq!(m.get(&1), Some(&15));
    }

    #[def_test]
    fn clear_removes_all() {
        let mut m = KHashMap::new();
        m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        m.try_insert(2, "b", AllocContext::Sleepable).unwrap();
        m.clear();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[def_test]
    fn iter_yields_pairs() {
        let mut m = KHashMap::new();
        m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        m.try_insert(2, "b", AllocContext::Sleepable).unwrap();
        let mut pairs: alloc::vec::Vec<_> = m.iter().map(|(&k, &v)| (k, v)).collect();
        pairs.sort_by_key(|p| p.0);
        assert_eq!(pairs, &[(1, "a"), (2, "b")]);
    }

    #[def_test]
    fn into_iter_consumes() {
        let mut m = KHashMap::new();
        m.try_insert(1, "a", AllocContext::Sleepable).unwrap();
        m.try_insert(2, "b", AllocContext::Sleepable).unwrap();
        let mut pairs: alloc::vec::Vec<_> = m.into_iter().collect();
        pairs.sort_by_key(|p| p.0);
        assert_eq!(pairs, &[(1, "a"), (2, "b")]);
    }

    #[def_test]
    fn try_reserve_grows_capacity() {
        let mut m = KHashMap::<u32, u32>::new();
        let old_cap = m.capacity();
        m.try_reserve(32, AllocContext::Sleepable).unwrap();
        assert!(m.capacity() >= old_cap + 32);
    }

    #[def_test]
    fn try_insert_rejects_unsupported_context() {
        let mut m = KHashMap::<u32, u32>::new();
        let err = m.try_insert(1, 2, AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test(serial)]
    fn try_insert_reports_injected_nomemory() {
        let _guard = kalloc::fault_injection::fail_global_alloc();
        let mut m = KHashMap::<u32, u32>::new();
        let err = m.try_insert(1, 2, AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
    }

    #[def_test]
    fn try_reserve_rejects_unsupported_context() {
        let mut m = KHashMap::<u32, u32>::new();
        let err = m.try_reserve(4, AllocContext::NoWait).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test]
    fn keys_and_values_iterators() {
        let mut m = KHashMap::new();
        m.try_insert("x", 1, AllocContext::Sleepable).unwrap();
        m.try_insert("y", 2, AllocContext::Sleepable).unwrap();
        let mut keys: alloc::vec::Vec<_> = m.keys().copied().collect();
        let mut values: alloc::vec::Vec<_> = m.values().copied().collect();
        keys.sort();
        values.sort();
        assert_eq!(keys, &["x", "y"]);
        assert_eq!(values, &[1, 2]);
    }
}
