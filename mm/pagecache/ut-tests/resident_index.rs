// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

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

#[def_test]
fn withdrawn_leaf_keeps_its_arc_until_the_reader_exits() {
    let (mut entries, index) = ResidentEntries::new();
    let original = folio(7);
    let weak = Arc::downgrade(&original);
    entries.insert(7, original);

    krcu::with_read(|scope| {
        let root = index.root.load(Ordering::Acquire);
        // SAFETY: this active kernel scope retains the initialized root even
        // after the following writer interleaving withdraws the entire tree.
        let node = unsafe { &*root };
        let Contents::Branch { slots, .. } = &node.contents else {
            unreachable!()
        };
        let leaf = slots[7].load(Ordering::Acquire);
        // SAFETY: the acquire-loaded leaf remains owned through this scope's
        // grace period; only immutable contents are borrowed.
        let old_leaf = unsafe { &*leaf };
        // Deterministic reader/writer interleaving: exclusive writer access
        // needs no mutex here, and removal neither allocates nor waits for GP.
        drop(entries.remove(&7));
        assert!(index.get(7, scope).is_none());
        let Contents::Leaf { folio, .. } = &old_leaf.contents else {
            unreachable!()
        };
        assert_eq!(Arc::strong_count(folio), 1);
        assert!(weak.upgrade().is_some());
    });
    drain_retirements();
    wait_for_release(|| weak.strong_count() == 0);
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
