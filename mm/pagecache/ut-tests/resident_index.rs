// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::def_test;

use super::*;
use crate::pagecache_unittest::wait_for_release;

fn folio(index: PageIndex) -> FolioRef {
    Arc::new(Folio::new_zeroed(index).unwrap())
}

fn drain_retirements() {
    // SAFETY: this test has finished all of its index mutations and holds no
    // read scope or lock. Callbacks free nodes and transfer folio references
    // to task workers; tests separately wait for their observable release.
    unsafe { krcu::drain_callbacks() }.unwrap();
}

#[def_test]
fn sparse_indices_survive_root_growth_and_neighbor_removal() {
    let (mut entries, index) = ResidentEntries::new();
    let keys = [0, 63, 64, 4095, 4096, 1 << 36, u64::MAX];
    assert!(krcu::with_read(|scope| index.get(0, scope)).is_none());
    for key in keys {
        entries.insert(key, folio(key));
        for (&resident, expected) in entries.iter() {
            let found = krcu::with_read(|scope| index.get(resident, scope)).unwrap();
            assert!(Arc::ptr_eq(&found, expected));
        }
    }
    for key in [1, 62, 65, 4094, 4097, u64::MAX - 1] {
        assert!(krcu::with_read(|scope| index.get(key, scope)).is_none());
    }
    for key in keys.into_iter().rev() {
        entries.remove(&key).unwrap();
        assert!(krcu::with_read(|scope| index.get(key, scope)).is_none());
        for (&resident, expected) in entries.iter() {
            let found = krcu::with_read(|scope| index.get(resident, scope)).unwrap();
            assert!(Arc::ptr_eq(&found, expected));
        }
    }
    assert!(index.root.load(Ordering::Acquire).is_null());
    drain_retirements();
}

// Keep the writer and a spinning RCU reader on distinct CPUs. The guard restores
// the test runner's affinity even if a test assertion returns early.
struct WriterAffinity(ktask::KCpuMask);

impl WriterAffinity {
    fn pin() -> Self {
        let saved = Self(ktask::current().cpumask());
        assert!(ktask::set_current_affinity(
            ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0),)
        ));
        saved
    }
}

impl Drop for WriterAffinity {
    fn drop(&mut self) {
        assert!(ktask::set_current_affinity(self.0));
    }
}

#[def_test]
fn withdrawn_nodes_survive_a_reader_on_another_cpu() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let _affinity = WriterAffinity::pin();
    // Exercise leaf removal, same-key replacement and whole-root withdrawal.
    for operation in 0..3 {
        let (mut entries, index) = ResidentEntries::new();
        let original = folio(7);
        let weak = Arc::downgrade(&original);
        entries.insert(7, original);
        let has_borrowed = Arc::new(AtomicBool::new(false));
        let can_exit = Arc::new(AtomicBool::new(false));
        let reader_index = index.clone();
        let reader_borrowed = has_borrowed.clone();
        let reader_exit = can_exit.clone();
        let reader = ktask::spawn(move || {
            assert!(ktask::set_current_affinity(
                ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1),)
            ));
            krcu::with_read(|_| {
                let root = reader_index.root.load(Ordering::Acquire);
                // SAFETY: publication preceded task creation and the RCU scope
                // retains the root and its children through writer withdrawal.
                let node = unsafe { &*root };
                let Contents::Branch { slots, .. } = &node.contents else {
                    unreachable!()
                };
                let leaf = slots[7].load(Ordering::Acquire);
                // SAFETY: this acquire-loaded child stays alive until scope exit.
                let old_leaf = unsafe { &*leaf };
                reader_borrowed.store(true, Ordering::Release);
                while !reader_exit.load(Ordering::Acquire) {
                    // No sleeping, voluntary yielding or allocation in RCU.
                    core::hint::spin_loop();
                }
                let Contents::Leaf { folio, .. } = &old_leaf.contents else {
                    unreachable!()
                };
                assert_eq!(folio.index(), 7);
                assert_eq!(Arc::strong_count(folio), 1);
            });
        });
        while !has_borrowed.load(Ordering::Acquire) {
            ktask::yield_now();
        }
        match operation {
            0 => drop(entries.remove(&7)),
            1 => drop(entries.insert(7, folio(7))),
            _ => drop(entries.take()),
        }
        // Grow a fresh/current root while the reader still borrows the old leaf.
        entries.insert(u64::MAX, folio(u64::MAX));
        let was_retained = weak.strong_count() == 1;
        can_exit.store(true, Ordering::Release);
        assert_eq!(reader.join(), 0);
        assert!(was_retained);
        for (&key, expected) in entries.iter() {
            let found = krcu::with_read(|scope| index.get(key, scope)).unwrap();
            assert!(Arc::ptr_eq(&found, expected));
        }
        drain_retirements();
        wait_for_release(|| weak.strong_count() == 0);
        drop(entries.take());
        drain_retirements();
    }
}

#[def_test]
fn concurrent_lookup_survives_growth_replacement_and_pruning() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let _affinity = WriterAffinity::pin();
    let (mut entries, index) = ResidentEntries::new();
    let keys = [0, 63, 64, 4096, 1 << 36, u64::MAX];
    let requested = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let is_done = Arc::new(AtomicBool::new(false));
    let reader_index = index.clone();
    let reader_requested = requested.clone();
    let reader_observed = observed.clone();
    let reader_done = is_done.clone();
    let reader = ktask::spawn(move || {
        assert!(ktask::set_current_affinity(
            ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1),)
        ));
        while !reader_done.load(Ordering::Acquire) {
            let phase = reader_requested.load(Ordering::Acquire);
            for key in keys {
                let found = krcu::with_read(|scope| reader_index.get(key, scope));
                if let Some(found) = found {
                    assert_eq!(found.index(), key);
                }
            }
            reader_observed.store(phase, Ordering::Release);
            ktask::yield_now();
        }
    });
    // Each phase is acknowledged by the live lookup task. Assertions concern
    // identity and reclamation, never a fixed number of yields or throughput.
    for phase in 1..=6 {
        for key in keys {
            match phase {
                1 | 2 | 4 | 6 => {
                    entries.insert(key, folio(key));
                }
                3 => {
                    entries.remove(&key);
                }
                _ => {
                    drop(entries.take());
                }
            }
        }
        requested.store(phase, Ordering::Release);
        while observed.load(Ordering::Acquire) != phase {
            ktask::yield_now();
        }
    }
    is_done.store(true, Ordering::Release);
    assert_eq!(reader.join(), 0);
    // A larger detached tree crosses the worker's yielding boundary. Include
    // dense pages alongside the sparse high keys and verify every owner releases.
    for key in 128..256 {
        entries.insert(key, folio(key));
    }
    for (&key, expected) in entries.iter() {
        let found = krcu::with_read(|scope| index.get(key, scope)).unwrap();
        assert!(Arc::ptr_eq(&found, expected));
    }
    let weak: alloc::vec::Vec<_> = entries.values().map(Arc::downgrade).collect();
    drop(entries.take());
    for key in keys {
        assert!(krcu::with_read(|scope| index.get(key, scope)).is_none());
    }
    drain_retirements();
    wait_for_release(|| weak.iter().all(|folio| folio.strong_count() == 0));
}

#[def_test]
fn replacement_preserves_owned_old_references_and_publishes_new_identity() {
    let (mut entries, index) = ResidentEntries::new();
    entries.insert(64, folio(64));
    let old = krcu::with_read(|scope| index.get(64, scope)).unwrap();
    let old_weak = Arc::downgrade(&old);
    let new = folio(64);
    drop(entries.insert(64, Arc::clone(&new)));
    drain_retirements();
    let found = krcu::with_read(|scope| index.get(64, scope)).unwrap();
    assert!(Arc::ptr_eq(&found, &new));
    assert!(!Arc::ptr_eq(&found, &old));
    assert!(old_weak.upgrade().is_some());
    drop(old);
    wait_for_release(|| old_weak.strong_count() == 0);
    drop(entries.take());
    drain_retirements();
}

#[def_test]
fn owned_reference_survives_final_index_destruction() {
    let owned = {
        let (mut entries, index) = ResidentEntries::new();
        entries.insert(u64::MAX, folio(u64::MAX));
        krcu::with_read(|scope| index.get(u64::MAX, scope)).unwrap()
    };
    assert_eq!(owned.lock().index(), u64::MAX);
    wait_for_release(|| Arc::strong_count(&owned) == 1);
}
