// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Registration ownership, consumer references and operation-permit tests.

use alloc::{format, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use unittest::{assert_eq, def_test};

use crate::{Class, ClassHooks, PublicationNode, PublicationSource, owner::LegacyOwner};

fn owner(id: u32) -> Arc<LegacyOwner> {
    LegacyOwner::new("test-driver", format!("platform/firmware{id}"))
}

#[def_test(serial)]
fn registration_withdraws_but_consumer_clones_do_not() {
    static CLASS: Class<u32> = Class::new(());
    let registration = CLASS.publish(owner(1), 7, "dev").expect("publish");
    let consumer = registration.publication();
    let clone = consumer.clone();
    drop(consumer);
    assert_eq!(CLASS.devices().len(), 1);
    assert_eq!(clone.with_available(|value| Ok(*value)), Ok(7));
    drop(registration);
    assert!(CLASS.devices().is_empty());
    assert!(!clone.is_available());
    assert_eq!(
        clone.with_available(|value| Ok(*value)),
        Err(KError::BadState)
    );
    assert!(clone.acquire_use().is_err());
    assert_eq!(clone.name(), "dev");
}

#[def_test(serial)]
fn class_teardown_waits_for_last_in_flight_use_not_last_reference() {
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);
    static CLASS: Class<u32, Hooks> = Class::new(Hooks);
    struct Hooks;
    impl ClassHooks<u32> for Hooks {
        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }
    UNPUBLISHED.store(0, Ordering::SeqCst);
    let registration = CLASS.publish(owner(2), 7, "drain").expect("publish");
    let consumer = registration.publication();
    let first_use = consumer.acquire_use().expect("first permit");
    let second_use = consumer.acquire_use().expect("second permit");
    drop(registration);
    assert!(!consumer.is_available());
    assert!(CLASS.devices().is_empty());
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 0);
    assert_eq!(first_use.with(|value| *value), 7);
    drop(first_use);
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 0);
    drop(second_use);
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
    assert!(consumer.acquire_use().is_err());
    drop(consumer);
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
}

#[def_test(serial)]
fn acquire_after_gate_close_is_refused_and_leaves_no_count() {
    static CLASS: Class<u32> = Class::new(());
    let registration = CLASS.publish(owner(3), 1, "gate").expect("publish");
    let consumer = registration.publication();
    drop(registration);
    for _ in 0..3 {
        assert!(consumer.acquire_use().is_err());
    }
    assert_eq!(consumer.in_flight(), 0);
}

#[def_test(serial)]
fn publication_withdrawal_does_not_retire_source_or_siblings() {
    static FIRST: Class<u32> = Class::new(());
    static SECOND: Class<u32> = Class::new(());
    let source = owner(4);
    let first = FIRST.publish(source.clone(), 1, "same").expect("first");
    let sibling = FIRST.publish(source.clone(), 2, "other").expect("sibling");
    let second = SECOND.publish(source.clone(), 3, "same").expect("second");
    let old = first.publication();
    drop(first);
    assert!(source.is_online());
    assert!(!old.is_available());
    assert_eq!(
        sibling.publication().with_available(|value| Ok(*value)),
        Ok(2)
    );
    assert_eq!(
        second.publication().with_available(|value| Ok(*value)),
        Ok(3)
    );
    let replacement = FIRST.publish(source.clone(), 4, "same").expect("republish");
    assert!(!old.same_publication(&replacement.publication()));
    source.mark_offline();
    assert!(sibling.publication().acquire_use().is_err());
    assert!(second.publication().acquire_use().is_err());
    assert!(replacement.publication().acquire_use().is_err());
    assert!(FIRST.devices().is_empty());
    assert_eq!(
        FIRST.publish(source, 5, "offline").unwrap_err(),
        KError::BadState
    );
}

#[def_test(serial)]
fn duplicate_publish_preserves_original_registration() {
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);
    static CLASS: Class<u32, Hooks> = Class::new(Hooks);
    struct Hooks;
    impl ClassHooks<u32> for Hooks {
        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }
    UNPUBLISHED.store(0, Ordering::SeqCst);
    let source = owner(5);
    let first = CLASS.publish(source.clone(), 1, "dev").expect("first");
    assert_eq!(
        CLASS.publish(source.clone(), 2, "dev").unwrap_err(),
        KError::AlreadyExists
    );
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
    assert!(source.is_online());
    assert_eq!(
        first.publication().with_available(|value| Ok(*value)),
        Ok(1)
    );
    drop(first);
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 2);
}

#[def_test(serial)]
fn publication_identity_is_instance_scoped() {
    static FIRST: Class<u32> = Class::new(());
    static SECOND: Class<u32> = Class::new(());
    let first = FIRST.publish(owner(6), 1, "same").expect("first");
    let borrowed = first.publication();
    let second = SECOND.publish(owner(7), 1, "same").expect("second");
    assert!(first.publication().same_publication(&borrowed));
    assert!(!borrowed.same_publication(&second.publication()));
    drop(first);
    assert_eq!(SECOND.devices().len(), 1);
    let replacement = FIRST.publish(owner(6), 1, "same").expect("replacement");
    assert!(!borrowed.same_publication(&replacement.publication()));
    assert!(borrowed.same_publication(&borrowed.clone()));
    drop(borrowed);
    assert_eq!(FIRST.devices().len(), 1);
}

#[def_test(serial)]
fn failed_hook_rolls_back_without_publishing_or_retiring_source() {
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);
    static CLASS: Class<u32, Hooks> = Class::new(Hooks);
    struct Hooks;
    impl ClassHooks<u32> for Hooks {
        fn publish(&self, _runtime: &u32, _node: &PublicationNode) -> KResult<()> {
            Err(KError::Io)
        }

        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }
    UNPUBLISHED.store(0, Ordering::SeqCst);
    let source = owner(8);
    assert_eq!(
        CLASS.publish(source.clone(), 1, "failure").unwrap_err(),
        KError::Io
    );
    assert!(source.is_online());
    assert!(CLASS.devices().is_empty());
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
}

#[def_test(serial)]
fn removal_callback_observes_closed_gate_and_may_release_last_use() {
    static CALLBACKS: AtomicUsize = AtomicUsize::new(0);
    static UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);
    static CLASS: Class<u32, Hooks> = Class::new(Hooks);
    struct Hooks;
    impl ClassHooks<u32> for Hooks {
        fn unpublish(&self, _runtime: &u32, _node: &PublicationNode) {
            UNPUBLISHED.fetch_add(1, Ordering::SeqCst);
        }
    }
    CALLBACKS.store(0, Ordering::SeqCst);
    UNPUBLISHED.store(0, Ordering::SeqCst);
    let registration = CLASS.publish(owner(9), 9, "callback").expect("publish");
    let permit = Arc::new(kspin::SpinNoPreempt::new(Some(
        registration.publication().acquire_use().expect("permit"),
    )));
    CLASS.subscribe_removed(Arc::new(move |publication| {
        let is_closed = !publication.is_available() && publication.acquire_use().is_err();
        let is_absent = CLASS.devices().is_empty();
        let last_use = permit.lock().take();
        drop(last_use);
        if is_closed && is_absent && UNPUBLISHED.load(Ordering::SeqCst) == 0 {
            CALLBACKS.fetch_add(1, Ordering::SeqCst);
        }
    }));
    drop(registration);
    assert_eq!(CALLBACKS.load(Ordering::SeqCst), 1);
    assert_eq!(UNPUBLISHED.load(Ordering::SeqCst), 1);
}

#[def_test(serial)]
fn payload_memory_outlives_registration_until_last_consumer_leaves() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    static CLASS: Class<Payload> = Class::new(());
    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }
    DROPS.store(0, Ordering::SeqCst);
    let registration = CLASS.publish(owner(10), Payload, "payload").unwrap();
    let consumer = registration.publication();
    drop(registration);
    assert_eq!(DROPS.load(Ordering::SeqCst), 0);
    assert!(consumer.acquire_use().is_err());
    drop(consumer);
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);
}
