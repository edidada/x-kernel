// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared byte storage with independently owned views.

use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

/// A byte view whose bounds can change without copying shared storage.
#[derive(Clone, Debug)]
pub(crate) struct PacketBuf {
    storage: Arc<PacketStorage>,
    head: usize,
    tail: usize,
}

#[derive(Clone, Debug)]
struct PacketStorage {
    data: Vec<u8>,
}

impl PacketBuf {
    /// Creates a view over newly owned bytes.
    pub(crate) fn new(data: Vec<u8>) -> Self {
        let tail = data.len();
        Self {
            storage: Arc::new(PacketStorage { data }),
            head: 0,
            tail,
        }
    }

    /// Returns the number of visible bytes.
    pub(crate) fn len(&self) -> usize {
        self.tail - self.head
    }

    /// Returns the visible bytes.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.storage.data[self.head..self.tail]
    }

    /// Returns writable bytes, copying the backing storage only when shared.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut Arc::make_mut(&mut self.storage).data[self.head..self.tail]
    }

    /// Restricts this view to a range relative to its current start.
    ///
    /// Returns `None` for a reversed or out-of-bounds range, leaving the view
    /// unchanged. Empty ranges are valid. Shared bytes are never copied.
    pub(crate) fn trim(&mut self, range: Range<usize>) -> Option<()> {
        if range.start > range.end || range.end > self.len() {
            return None;
        }
        self.tail = self.head + range.end;
        self.head += range.start;
        Some(())
    }

    #[cfg(unittest)]
    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.storage, &other.storage)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::def_test;

    use super::*;

    #[def_test]
    fn trimming_a_view_keeps_shared_bytes_and_checks_bounds() {
        let packet = PacketBuf::new(vec![1, 2, 3, 4]);
        let mut view = packet.clone();
        assert_eq!(view.trim(1..3), Some(()));
        assert_eq!(view.as_slice(), &[2, 3]);
        assert_eq!(packet.as_slice(), &[1, 2, 3, 4]);
        assert!(view.shares_storage_with(&packet));

        for range in [Range { start: 2, end: 1 }, 0..3, 0..usize::MAX] {
            assert_eq!(view.trim(range), None);
            assert_eq!(view.as_slice(), &[2, 3]);
            assert!(view.shares_storage_with(&packet));
        }
        assert_eq!(view.trim(1..2), Some(()));
        assert_eq!(view.as_slice(), &[3]);
        assert_eq!(view.trim(1..1), Some(()));
        assert_eq!(view.len(), 0);
        assert!(view.shares_storage_with(&packet));
    }

    #[def_test]
    fn writing_a_shared_view_copies_only_at_byte_mutation() {
        let packet = PacketBuf::new(vec![1, 2, 3, 4]);
        let mut view = packet.clone();
        view.trim(1..3).unwrap();
        assert!(view.shares_storage_with(&packet));
        view.as_mut_slice()[0] = 9;
        assert!(!view.shares_storage_with(&packet));
        assert_eq!(packet.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(view.as_slice(), &[9, 3]);
    }
}
