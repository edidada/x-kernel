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

use kerrno::{KError, KResult};
use unittest::{assert_eq, def_test};

use crate::{
    Class, ClassHooks, Publication, PublicationId, PublicationNode, PublicationOwner,
    owner::LegacyOwner,
};

impl<T> Publication<T> {
    /// Detached publication for tests: its own device identity, no registry,
    /// no withdrawal. `offline` mirrors an already-removed device.
    pub fn detached(
        _device_id: u64,
        driver_name: &'static str,
        path: String,
        runtime: T,
        name: &str,
        offline: bool,
    ) -> Self {
        let owner = LegacyOwner::new(driver_name, path);
        if offline {
            owner.mark_offline();
        }
        Self::new(owner, runtime, String::from(name), PublicationId::next())
    }
}

fn loc(id: u32) -> String {
    format!("platform/firmware{id}")
}

fn owner(id: u32) -> Arc<LegacyOwner> {
    LegacyOwner::new("test-driver", loc(id))
}

/// One publication: the value `publish` returns is the owner; clones handed
/// to consumers are borrows and never withdraw.
#[def_test(serial)]
fn publication_owner_withdraws_and_clones_do_not() {
    static CLASS: Class<u32, NoHooks> = Class::new(NoHooks);
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);

    struct NoHooks;
    impl ClassHooks<u32> for NoHooks {
        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }

    UNPUBLISHED.store(0, Ordering::SeqCst);
    let published = CLASS.publish(owner(1), 7u32, "dev").expect("publish");
    let id = published.id();
    let borrow = published.clone();
    assert_eq!(CLASS.find(id).map(|p| p.with(|v| *v)), Some(7));

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
        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {
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
    let before =
        borrowed.with_available(|runtime| Ok::<u32, kerrno::KError>(*runtime) as KResult<u32>);
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

/// One class serves many devices (class 1:N), and one device identity can be
/// the owner of publications in several classes (device/class N:M).
///
/// Liveness is shared: publication availability reads the device's online
/// flag, so withdrawing ONE publication marks the whole device offline — the
/// other classes' entries stay registered but stop being available, and their
/// own drop completes the withdrawal. That is the model's unit of life: one
/// device, not one publication.
#[def_test(serial)]
fn one_class_serves_many_devices_and_publications_share_device_liveness() {
    static CHAR_LIKE: Class<u32, NoHooks> = Class::new(NoHooks);
    static BLOCK_LIKE: Class<u64, NoHooks> = Class::new(NoHooks);

    struct NoHooks;
    impl ClassHooks<u32> for NoHooks {}
    impl ClassHooks<u64> for NoHooks {}

    let first = CHAR_LIKE.publish(owner(10), 1, "a").expect("publish a");
    let second = CHAR_LIKE.publish(owner(11), 2, "b").expect("publish b");
    assert_eq!(CHAR_LIKE.devices().len(), 2);

    // One device, two classes: the owner identity is shared, the handles are
    // distinct.
    let device = owner(12);
    let char_side = CHAR_LIKE
        .publish(device.clone(), 3, "c")
        .expect("publish c");
    let block_side = BLOCK_LIKE.publish(device, 4, "d").expect("publish d");
    assert_ne!(char_side.id(), block_side.id());
    assert_eq!(char_side.location(), block_side.location());

    // Withdrawing one publication flips the shared device identity: the
    // sibling is no longer handed out ...
    drop(char_side);
    assert!(BLOCK_LIKE.find(block_side.id()).is_none());
    // ... but its runtime stays reachable for its own teardown, and its drop
    // removes the registry entry.
    assert_eq!(block_side.with(|value| *value), 4);
    drop(block_side);
    assert_eq!(BLOCK_LIKE.devices().len(), 0);

    drop(first);
    drop(second);
}

/// Publishing the same runtime name twice off the same owning device is a
/// driver bug and must be refused with `AlreadyExists` — the registry's
/// duplicate-identity contract (same name under a different owner is fine).
#[def_test(serial)]
fn duplicate_publish_of_same_owner_and_name_is_refused() {
    static CLASS: Class<u32, NoHooks2> = Class::new(NoHooks2);
    struct NoHooks2;
    impl ClassHooks<u32> for NoHooks2 {
        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {}
    }

    let host = owner(9);
    // Hold the owning publication: dropping it would withdraw the entry
    // and the duplicate test below would see an empty registry.
    let first = CLASS
        .publish(host.clone(), 1u32, "dev")
        .expect("first publish must succeed");
    let second = CLASS.publish(host, 2u32, "dev");
    assert!(matches!(second, Err(KError::AlreadyExists)));

    // A different owner may reuse the name (Linux: same name under a
    // different parent is a different device).
    let other = CLASS.publish(owner(10), 3u32, "dev");
    assert!(
        other.is_ok(),
        "same name under a different owner is allowed"
    );
    drop(first);
}
