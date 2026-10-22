// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One published class device: the value a driver holds, and the object
//! consumers use.
//!
//! This module used to carry four names for one thing
//! (`ClassParent` + `ClassDevice` + `ClassWithdraw` + `ClassUse`). They are
//! one type now:
//!
//! * [`Publication<T>`] is the value [`Class::publish`] returns. It owns the
//!   withdrawal (dropping the **owning** handle unpublishes, after every
//!   in-flight [`InUse`] has left), the runtime payload `T`, the identity of
//!   the device that published it, and the devnode projection.
//! * [`InUse<T>`] is the counted borrow a data-plane operation takes
//!   (`acquire_use`). It keeps the payload alive and runs the class-side
//!   teardown when the last one leaves.
//!
//! Clones of a [`Publication`] that consumers receive from
//! [`Class::devices`]/[`Class::find`]/subscriptions are **borrows**: they
//! never withdraw. Only the value produced by `publish` is the owner.
//!
//! [`Class::publish`]: crate::class_api::Class::publish
//! [`Class::devices`]: crate::class_api::Class::devices
//! [`Class::find`]: crate::class_api::Class::find

#![no_std]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use kerrno::{KError, KResult};

mod owner {
    //! Live identity supplied by the device model or the legacy publisher bridge.

    use alloc::{string::String, sync::Arc};
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Device-model contract for a publication's shared live identity.
    ///
    /// The model retains ownership of the state and must reuse the same `Arc`
    /// allocation for all publications of one device. Class registries compare
    /// allocation identity, not paths or numeric identifiers, for duplicate detection.
    /// Methods run outside the class registry lock in task context; they must not
    /// reenter publication withdrawal. The path must remain stable for this identity.
    pub trait PublicationOwner: Send + Sync + 'static {
        /// Driver currently claiming the device; `None` prevents publication.
        fn driver_name(&self) -> Option<&'static str>;

        /// Stable physical path used by class projections.
        fn physical_path(&self) -> &str;

        /// Whether the device still backs its published capabilities.
        fn is_online(&self) -> bool;

        /// Permanently retire this identity after a publication is withdrawn.
        ///
        /// This idempotent model callback preserves the shared-device retirement
        /// protocol: all publications observe the same offline state. It must not
        /// free runtime payloads; class teardown waits for its own in-flight users.
        fn mark_offline(&self);
    }

    pub(crate) struct LegacyOwner {
        driver_name: &'static str,
        physical_path: String,
        offline: AtomicBool,
    }

    impl LegacyOwner {
        pub(crate) fn new(driver_name: &'static str, physical_path: String) -> Arc<Self> {
            Arc::new(Self {
                driver_name,
                physical_path,
                offline: AtomicBool::new(false),
            })
        }
    }

    impl PublicationOwner for LegacyOwner {
        fn driver_name(&self) -> Option<&'static str> {
            Some(self.driver_name)
        }

        fn physical_path(&self) -> &str {
            &self.physical_path
        }

        fn is_online(&self) -> bool {
            !self.offline.load(Ordering::Acquire)
        }

        fn mark_offline(&self) {
            self.offline.store(true, Ordering::Release);
        }
    }
}

pub use class_api::{Class, ClassHooks, PublicationNode};
pub use owner::PublicationOwner;
pub use publication::{InUse, Publication, PublicationId};

mod class_api;
mod publication;

/// Notification callback invoked after a class device is published while its
/// owning device is available.
pub type ClassAvailabilityCallback<T> = Arc<dyn Fn(Publication<T>) + Send + Sync>;

/// Notification callback invoked when a class device is removed.
pub type ClassRemovalCallback<T> = Arc<dyn Fn(Publication<T>) + Send + Sync>;

/// Typed registry for one runtime device class.
///
/// Holds borrows, not owners: the withdrawal belongs to the driver's
/// `Publication`. Entries exist exactly between `publish` and that drop.
pub(crate) struct ClassRegistry<T> {
    devices: Vec<Publication<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    removal_subscribers: Vec<ClassRemovalCallback<T>>,
}

impl<T> ClassRegistry<T> {
    pub(crate) const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
            removal_subscribers: Vec::new(),
        }
    }

    /// Publish one runtime class device.
    ///
    /// Linux device registration rejects duplicate identities; replacement is
    /// a remove-then-add lifecycle rather than an implicit registry update.
    /// Publication ids are minted fresh per publish, so the duplicate test is
    /// (owning device identity, runtime name) — the same device publishing
    /// the same name twice is a driver bug and is refused.
    pub(crate) fn publish(&mut self, device: Publication<T>) -> KResult<()> {
        let duplicate = self
            .devices
            .iter()
            .any(|current| current.shares_identity_with(&device));
        if duplicate {
            return Err(KError::AlreadyExists);
        }
        self.devices.push(device);
        Ok(())
    }

    /// Every available device in this class.
    pub(crate) fn devices(&self) -> Vec<Publication<T>> {
        self.devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Find one device by its publication handle.
    pub(crate) fn find(&self, id: PublicationId) -> Option<Publication<T>> {
        self.devices
            .iter()
            .find(|device| device.id() == id && device.is_available())
            .cloned()
    }

    pub(crate) fn remove(&mut self, id: PublicationId) -> Option<Publication<T>> {
        let position = self.devices.iter().position(|device| device.id() == id)?;
        Some(self.devices.swap_remove(position))
    }

    pub(crate) fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    pub(crate) fn subscribe_removed(&mut self, callback: ClassRemovalCallback<T>) {
        self.removal_subscribers.push(callback);
    }

    pub(crate) fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    pub(crate) fn removal_callbacks(&self) -> Vec<ClassRemovalCallback<T>> {
        self.removal_subscribers.clone()
    }
}

unittest::path_mod_test!(tests, "../ut-tests/class.rs");
