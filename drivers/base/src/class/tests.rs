// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic class-machinery tests (no device family involved).
//!
//! These run in the kernel unit-test harness (`#[cfg(unittest)]`), not in a
//! host `cargo test`: the machinery they cover is the one the kernel
//! actually boots with.

use alloc::{format, string::String, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::KResult;
use unittest::{assert_eq, def_test};

use super::{class_api::{Class, ClassHooks}, *};
use crate::{DeviceCore, DeviceId};

fn loc(id: u32) -> String {
    format!("platform/firmware{id}")
}

fn owner(id: u32) -> Arc<DeviceCore> {
    DeviceCore::synthetic(DeviceId(id as u64), "test-driver", loc(id))
}

/// One publication: the value `publish` returns is the owner; clones handed
/// to consumers are borrows and never withdraw.
#[def_test(serial)]
fn publication_owner_withdraws_and_clones_do_not() {
    static CLASS: Class<u32, NoHooks> = Class::new(NoHooks);
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);

    struct NoHooks;
    impl ClassHooks<u32> for NoHooks {
        fn unpublish(&self, _runtime: &u32) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }

    UNPUBLISHED.store(0, Ordering::SeqCst);
    let published = CLASS.publish(owner(1), 7u32, "dev").expect("publish");
    let id = published.id();
    let borrow = published.clone();
    assert_eq!(CLASS.find(id).map(|p| *p.with(|v| *v)), Some(7));

    // Consumer borrows never withdraw.
    drop(borrow);
    assert!(CLASS.find(id).is_some());
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 0);

    // The owning handle does.
    drop(published);
    assert!(CLASS.find(id).is_none());
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
}

/// The class-side teardown slot (`del_gendisk`, transport close) must not run
/// while an operation that already passed the availability gate is still in
/// flight: the last `InUse` drop runs it instead, exactly once.
#[def_test(serial)]
fn class_teardown_waits_for_last_in_flight_use() {
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);
    static CLASS: Class<u32, DrainHooks> = Class::new(DrainHooks);

    struct DrainHooks;
    impl ClassHooks<u32> for DrainHooks {
        fn unpublish(&self, _runtime: &u32) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }

    UNPUBLISHED.store(0, Ordering::SeqCst);
    let publication = CLASS.publish(owner(2), 7u32, "drain").expect("publish");
    let id = publication.id();

    let in_flight = publication.acquire_use().expect("acquire");
    assert_eq!(publication.in_flight(), 1);

    // Dropping the owning handle withdraws the publication immediately ...
    drop(publication);
    assert!(CLASS.find(id).is_none());
    assert!(!in_flight.is_available());
    assert!(in_flight.acquire_use().is_err());
    // ... but the class-side teardown waits for the live operation.
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 0);

    drop(in_flight);
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
}

/// The gate is re-checked after the in-flight count is bumped: an acquire
/// that overlaps a withdrawal either wins the count (and defers teardown) or
/// loses the gate and undoes its count.
#[def_test(serial)]
fn acquire_after_gate_close_is_refused_and_leaves_no_count() {
    static CLASS: Class<u32, NoHooks> = Class::new(NoHooks);

    struct NoHooks;
    impl ClassHooks<u32> for NoHooks {}

    let publication = CLASS.publish(owner(3), 1u32, "gate").expect("publish");
    let borrowed = publication.clone();
    drop(publication);

    for _ in 0..3 {
        assert!(borrowed.acquire_use().is_err());
    }
    assert_eq!(borrowed.in_flight(), 0);
}

/// `with_available` is the only gated data-plane entry: it must refuse after
/// withdrawal instead of calling into the runtime.
#[def_test(serial)]
fn with_available_refuses_after_withdrawal() {
    static CLASS: Class<u32, NoHooks> = Class::new(NoHooks);

    struct NoHooks;
    impl ClassHooks<u32> for NoHooks {}

    let publication = CLASS.publish(owner(4), 9u32, "gate2").expect("publish");
    let borrowed = publication.clone();
    let before = borrowed.with_available(|runtime| Ok::<u32, kerrno::KError>(*runtime) as KResult<u32>);
    assert_eq!(before.ok(), Some(9));

    drop(publication);
    assert!(borrowed.with_available(|_| Ok(0u32)).is_err());
}

/// Publication ids are freshly allocated, and the owner identity is the
/// device's — not a copy: two publications of one device share liveness and
/// DEVPATH, but have distinct handles.
#[def_test(serial)]
fn publications_share_the_owner_identity_but_not_the_handle() {
    static CLASS: Class<u32, NoHooks> = Class::new(NoHooks);

    struct NoHooks;
    impl ClassHooks<u32> for NoHooks {}

    let device = owner(5);
    let first = CLASS.publish(device.clone(), 1u32, "a").expect("publish a");
    let second = CLASS.publish(device, 2u32, "b").expect("publish b");

    assert!(first.id() != second.id());
    assert_eq!(first.location(), second.location());
    assert_eq!(first.driver_name(), Some("test-driver"));
    first.mark_offline();
    assert!(!first.is_available());
    assert!(!second.is_available());
}
