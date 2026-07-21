// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible UTF-8 string type ([`KString`]) built on top of [`KVec<u8>`](crate::KVec).
//!
//! `KString` provides the same fundamental operations as `alloc::string::String`
//! (push, push_str, formatting) but every operation that may grow the buffer
//! takes an explicit [`AllocContext`](crate::AllocContext) and returns
//! `Result<_, AllocError>` instead of panicking on out-of-memory.

use core::{cmp::Ordering, fmt, ops::Deref, str};

use crate::{AllocContext, AllocError, KVec};

/// A fallible, growable UTF-8 string.
///
/// `KString` wraps a [`KVec<u8>`] and maintains the invariant that its byte
/// buffer is always valid UTF-8. All mutating methods that may allocate take
/// an explicit `AllocContext` and return `Result`.
///
/// # UTF-8 invariant
///
/// All mutating methods only write valid UTF-8 byte sequences. The internal
/// buffer is never exposed mutably, so this invariant cannot be violated by
/// external code. `as_str()` is `unsafe` internally but safe to call because
/// the type upholds the invariant.
pub struct KString {
    inner: KVec<u8>,
}

impl KString {
    /// Constructs an empty string without allocating.
    pub const fn new() -> Self {
        Self { inner: KVec::new() }
    }

    /// Constructs an empty string with at least the given capacity.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the pre-allocation, or [`AllocError::UnsupportedContext`] when
    /// the caller requests a context reserved for a later phase.
    pub fn try_with_capacity(capacity: usize, context: AllocContext) -> Result<Self, AllocError> {
        let inner = KVec::try_with_capacity(capacity, context)?;
        Ok(Self { inner })
    }

    /// Creates a `KString` by copying the bytes of `s`.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when the kernel allocator cannot
    /// satisfy the allocation.
    pub fn try_from_str(s: &str, context: AllocContext) -> Result<Self, AllocError> {
        let mut inner = KVec::try_with_capacity(s.len(), context)?;
        // SAFETY: try_extend_from_slice for u8: Copy is available. The source
        // is valid bytes of a &str.
        // On error, inner is dropped (RAII), no partial state leaked.
        inner.try_extend_from_slice(s.as_bytes(), context)?;
        Ok(Self { inner })
    }

    /// Creates a `KString` from a `KVec<u8>`, validating UTF-8.
    ///
    /// Returns `Err(kvec)` if the bytes are not valid UTF-8.
    pub fn from_utf8(kvec: KVec<u8>) -> Result<Self, KVec<u8>> {
        if core::str::from_utf8(&kvec).is_ok() {
            Ok(Self { inner: kvec })
        } else {
            Err(kvec)
        }
    }

    /// Creates a `KString` from a byte vector without checking for valid UTF-8.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `kvec` contains valid UTF-8. Violating
    /// this invariant leads to UB in [`as_str`](Self::as_str) and anything
    /// that dereferences to `&str`.
    pub unsafe fn from_utf8_unchecked(kvec: KVec<u8>) -> Self {
        Self { inner: kvec }
    }

    /// Returns the length of the string in bytes (not chars).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the string is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the current allocated capacity.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Returns the string as a `&str`.
    ///
    /// # Safety (internal)
    ///
    /// `unsafe` because it calls `str::from_utf8_unchecked`. This is sound
    /// because the `KString` invariant guarantees the buffer always contains
    /// valid UTF-8.
    pub fn as_str(&self) -> &str {
        // SAFETY: The KString invariant guarantees self.inner contains only
        // valid UTF-8 bytes. All mutating methods only write valid UTF-8.
        unsafe { str::from_utf8_unchecked(self.inner.as_slice()) }
    }

    /// Returns the string's byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_slice()
    }

    /// Appends a single `char` to the string.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    pub fn try_push(&mut self, ch: char, context: AllocContext) -> Result<(), AllocError> {
        // Encode the char into a small stack buffer, then push the bytes.
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf);
        self.try_push_str(encoded, context)
    }

    /// Appends a string slice to this `KString`.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    pub fn try_push_str(&mut self, s: &str, context: AllocContext) -> Result<(), AllocError> {
        // u8: Copy, so try_extend_from_slice is available.
        self.inner.try_extend_from_slice(s.as_bytes(), context)
    }

    /// Appends formatted output to this `KString` using `fmt::Write`.
    ///
    /// This is the preferred way to replace `format!(...)` in fallible
    /// contexts, because it provides an explicit `AllocContext`.
    ///
    /// Only [`AllocContext::Sleepable`] is supported in phase 1.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::NoMemory`] when growth is required and the kernel
    /// allocator cannot satisfy it.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut s = KString::try_with_capacity(64, AllocContext::Sleepable)?;
    /// s.try_push_fmt(
    ///     format_args!("value={} status={}", 42, "ok"),
    ///     AllocContext::Sleepable,
    /// )?;
    /// ```
    pub fn try_push_fmt(
        &mut self,
        args: fmt::Arguments<'_>,
        context: AllocContext,
    ) -> Result<(), AllocError> {
        struct Adapter<'a> {
            s: &'a mut KString,
            context: AllocContext,
            result: Result<(), AllocError>,
        }

        impl fmt::Write for Adapter<'_> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                match self.s.try_push_str(s, self.context) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        self.result = Err(e);
                        Err(fmt::Error)
                    }
                }
            }
        }

        let mut adapter = Adapter {
            s: self,
            context,
            result: Ok(()),
        };

        // If fmt::Write::write_fmt fails, adapter.result captures the error.
        let _ = fmt::Write::write_fmt(&mut adapter, args);
        adapter.result
    }

    /// Removes all characters, preserving the allocated buffer for reuse.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

// -- Trait implementations -----------------------------------------------

impl Default for KString {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::TryClone for KString {
    fn try_clone(&self) -> Result<Self, AllocError> {
        Ok(Self {
            inner: self.inner.try_clone()?,
        })
    }
}

impl fmt::Debug for KString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for KString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl Deref for KString {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for KString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for KString {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl PartialEq for KString {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for KString {}

impl PartialEq<str> for KString {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for KString {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialOrd for KString {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for KString {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl core::hash::Hash for KString {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Drop for KString {
    fn drop(&mut self) {
        // KVec<u8>::drop handles deallocation.
    }
}

// SAFETY: KString wraps KVec<u8>, which is Send (u8: Send). The UTF-8
// invariant does not introduce shared mutable state.
unsafe impl Send for KString {}

// SAFETY: &KString provides only &str through Deref, and &str: Sync.
// The UTF-8 invariant is read-only and does not prevent sharing.
unsafe impl Sync for KString {}

impl From<KString> for crate::KVec<u8> {
    fn from(k: KString) -> Self {
        let k = core::mem::ManuallyDrop::new(k);
        // SAFETY: ManuallyDrop prevents double-free. We take ownership
        // of the inner KVec, which is a valid buffer from kalloc.
        unsafe { core::ptr::read(&k.inner) }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;
    use crate::fault_injection::{fail_allocations_after, fail_next_allocation};

    #[def_test]
    fn new_is_empty() {
        let s = KString::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.as_str(), "");
    }

    #[def_test]
    fn try_from_str_stores_and_derefs() {
        let s = KString::try_from_str("hello", AllocContext::Sleepable).unwrap();
        assert_eq!(s.len(), 5);
        assert_eq!(s.as_str(), "hello");
        // Deref to &str
        let r: &str = &s;
        assert_eq!(r, "hello");
    }

    #[def_test]
    fn try_with_capacity_preallocates() {
        let s = KString::try_with_capacity(64, AllocContext::Sleepable).unwrap();
        assert!(s.is_empty());
        assert!(s.capacity() >= 64);
    }

    #[def_test]
    fn try_push_appends_char() {
        let mut s = KString::new();
        s.try_push('A', AllocContext::Sleepable).unwrap();
        s.try_push('B', AllocContext::Sleepable).unwrap();
        assert_eq!(s.as_str(), "AB");
    }

    #[def_test]
    fn try_push_unicode_char() {
        let mut s = KString::new();
        // 3-byte UTF-8 character
        s.try_push('€', AllocContext::Sleepable).unwrap();
        assert_eq!(s.as_str(), "€");
        assert_eq!(s.len(), 3);
    }

    #[def_test]
    fn try_push_str_appends_slice() {
        let mut s = KString::try_from_str("abc", AllocContext::Sleepable).unwrap();
        s.try_push_str("def", AllocContext::Sleepable).unwrap();
        assert_eq!(s.as_str(), "abcdef");
    }

    #[def_test]
    fn try_push_fmt_append_formatted() {
        let mut s = KString::try_with_capacity(32, AllocContext::Sleepable).unwrap();
        s.try_push_fmt(
            format_args!("x={}, y={}", 10, "ok"),
            AllocContext::Sleepable,
        )
        .unwrap();
        assert_eq!(s.as_str(), "x=10, y=ok");
    }

    #[def_test]
    fn clear_empties_string_but_preserves_capacity() {
        let mut s = KString::try_from_str("hello world", AllocContext::Sleepable).unwrap();
        let cap = s.capacity();
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.as_str(), "");
        assert_eq!(s.capacity(), cap);
    }

    #[def_test]
    fn display_format_matches_str() {
        let s = KString::try_from_str("test", AllocContext::Sleepable).unwrap();
        assert_eq!(alloc::format!("{}", s), "test");
    }

    #[def_test]
    fn debug_format_quotes_string() {
        let s = KString::try_from_str("dbg", AllocContext::Sleepable).unwrap();
        let formatted = alloc::format!("{:?}", s);
        assert!(formatted.contains("dbg"));
    }

    // into_string removed — KString replaces String in all paths.

    #[def_test]
    fn as_ref_str() {
        let s = KString::try_from_str("ref", AllocContext::Sleepable).unwrap();
        let r: &str = s.as_ref();
        assert_eq!(r, "ref");
    }

    #[def_test]
    fn as_ref_bytes() {
        let s = KString::try_from_str("bytes", AllocContext::Sleepable).unwrap();
        let b: &[u8] = s.as_ref();
        assert_eq!(b, b"bytes");
    }

    #[def_test]
    fn try_from_str_rejects_unsupported_context() {
        let err = KString::try_from_str("test", AllocContext::Atomic).unwrap_err();
        assert_eq!(err, AllocError::UnsupportedContext);
    }

    #[def_test(serial)]
    fn try_from_str_reports_injected_nomemory() {
        let _guard = fail_next_allocation();
        let err = KString::try_from_str("test", AllocContext::Sleepable).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
    }

    #[def_test(serial)]
    fn try_push_str_growth_failure_preserves_old_contents() {
        let mut s = KString::try_from_str("hello", AllocContext::Sleepable).unwrap();

        let _guard = fail_allocations_after(0);
        let err = s
            .try_push_str(" world", AllocContext::Sleepable)
            .unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        // The old content must still be intact.
        assert_eq!(s.as_str(), "hello");
    }
}
