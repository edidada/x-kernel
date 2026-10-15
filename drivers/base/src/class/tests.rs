// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic class-machinery tests (no family involved). The family
//! sentinels — drain, lease-across-removal — live in `device-class`.

use unittest::{assert_eq, def_test};

use super::*;

fn loc(id: u32) -> alloc::string::String {
    alloc::format!("platform/firmware{id}")
}

/// Regression: the class publication ↔ driver-core owner association
/// used to be derived by clearing a tag bit off an independently
/// allocated publication id — an off-by-one that made remove(0) leak
/// device 0's publication and remove(1) delete device 0's. The owner
/// is explicit now; this pins the correlation.
#[def_test(serial)]
fn owner_correlation_is_explicit_not_derived() {
    // Two devices with domain ids 0 and 1 — exactly the values the
    // broken derivation shifted between.
    let a = ClassParent::detached(0, "test-a", loc(0));
    let b = ClassParent::detached(1, "test-b", loc(1));

    // The owner each publication reports is the one it was created
    // with, not id-derived arithmetic: the old code reported 1 and 2.
    assert_eq!(a.detached_owner(), Some(0));
    assert_eq!(b.detached_owner(), Some(1));
    // Distinct devices never collide on owner.
    assert!(a.detached_owner() != b.detached_owner());
}

/// Regression: the registry drain used to take only the FIRST registry
/// hit per owner — a device publishing two class devices of one class
/// leaked the second. The registry drain itself is what the aggregate
/// remove loops on; pin it here with a family-free registry.
#[def_test(serial)]
fn same_owner_multiple_publications_all_removable() {
    let mut registry: ClassRegistry<u32> = ClassRegistry::default();
    let a =
        ClassDevice::try_new_with_info(ClassParent::detached(9, "test", loc(0)), 1u32, "a".into())
            .unwrap();
    let b =
        ClassDevice::try_new_with_info(ClassParent::detached(9, "test", loc(1)), 2u32, "b".into())
            .unwrap();
    registry.publish(a).unwrap();
    registry.publish(b).unwrap();

    // Drain every publication of owner 9 — the second hit must be
    // reachable after the first removal (the old single-find bug).
    let mut drained = 0;
    while let Some(id) = registry.find_detached(9) {
        registry.remove(id);
        drained += 1;
    }
    assert_eq!(drained, 2);
    assert!(registry.find_detached(9).is_none());
}

/// Publication ids are freshly allocated and need not correlate with
/// owner ids in any way — two parents with the same owner shape still
/// get distinct publication ids.
#[def_test(serial)]
fn publication_id_is_an_independent_concept() {
    let a = ClassParent::detached(7, "test", loc(0));
    let b = ClassParent::detached(7, "test", loc(1));
    let (id_a, id_b) = match (&a, &b) {
        (ClassParent::Detached(da), ClassParent::Detached(db)) => (da.id, db.id),
    };
    assert!(id_a != id_b);
    assert_eq!(a.detached_owner(), Some(7));
    assert_eq!(b.detached_owner(), Some(7));
}
