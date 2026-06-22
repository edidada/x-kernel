// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{AllocContext, AllocError, KVec};

/// Collects an iterator into [`KVec`] without using panic-on-OOM collection.
///
/// Phase 1 fixes the collection context to [`AllocContext::Sleepable`]. The
/// partially built vector remains local to this helper, so any allocation
/// failure drops intermediate state instead of exposing a partially committed
/// public container.
///
/// # Errors
///
/// Returns [`AllocError::NoMemory`] when the kernel allocator cannot satisfy
/// the required vector growth during collection.
pub fn try_collect_vec<I>(iter: I) -> Result<KVec<I::Item>, AllocError>
where
    I: IntoIterator,
{
    let iter = iter.into_iter();
    let (lower_bound, _) = iter.size_hint();
    let mut vec = KVec::try_with_capacity(lower_bound, AllocContext::Sleepable)?;

    for item in iter {
        vec.try_push(item, AllocContext::Sleepable)?;
    }

    Ok(vec)
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::try_collect_vec;
    use crate::{AllocError, fault_injection::fail_allocations_after};

    #[derive(Debug)]
    struct DropTracker<'a> {
        id: usize,
        drops: &'a AtomicUsize,
        sum: &'a AtomicUsize,
    }

    impl Drop for DropTracker<'_> {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.sum.fetch_add(self.id, Ordering::SeqCst);
        }
    }

    #[def_test]
    fn collects_iterator_contents_in_order() {
        let vec = try_collect_vec([1_u32, 2, 3, 5]).unwrap();
        assert_eq!(vec.as_slice(), &[1, 2, 3, 5]);
    }

    #[def_test]
    fn collects_empty_iterator() {
        let vec = try_collect_vec(core::iter::empty::<u32>()).unwrap();
        assert!(vec.is_empty());
        assert_eq!(vec.as_slice(), &[]);
    }

    #[def_test(serial)]
    fn injected_collection_failure_reports_nomemory_and_drops_partial_state() {
        let drop_count = AtomicUsize::new(0);
        let drop_sum = AtomicUsize::new(0);
        let items = [
            DropTracker {
                id: 1,
                drops: &drop_count,
                sum: &drop_sum,
            },
            DropTracker {
                id: 2,
                drops: &drop_count,
                sum: &drop_sum,
            },
            DropTracker {
                id: 3,
                drops: &drop_count,
                sum: &drop_sum,
            },
            DropTracker {
                id: 4,
                drops: &drop_count,
                sum: &drop_sum,
            },
            DropTracker {
                id: 5,
                drops: &drop_count,
                sum: &drop_sum,
            },
        ];

        let _guard = fail_allocations_after(1);
        let err = try_collect_vec(items.into_iter().filter(|_| true)).unwrap_err();
        assert_eq!(err, AllocError::NoMemory);
        assert_eq!(drop_count.load(Ordering::SeqCst), 5);
        assert_eq!(drop_sum.load(Ordering::SeqCst), 15);
    }
}
