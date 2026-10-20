// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The one provider-registration convention.
//!
//! Linux has one shape for "a driver publishes a resource other drivers
//! consume": `of_clk_add_hw_provider`, `gpiochip_add_data`,
//! `regulator_register`, each registering a lookup keyed by the firmware
//! handle its consumers defer on. Consumers then return `-EPROBE_DEFER`
//! until that key appears.
//!
//! Transcribing those drivers mechanically needs the same shape here, and
//! exactly one of it:
//!
//! ```text
//! provider driver, end of probe:   dev.env().provide::<ClkProvider>(phandle, lookup)?
//! consumer driver:                 dev.res().clk("baudclk")?   // -EPROBE_DEFER while absent
//! ```
//!
//! A provider family (clock today; GPIO, regulator, pinctrl next) is a
//! [`ProviderClass`] descriptor naming its key type, its lookup trait, and
//! the [`DepKind`] it wakes. The descriptor lives with the family (on the
//! host side when the lookup trait is a host trait), never in this crate —
//! `driver-base` stays family-agnostic (L0-a) and host-free (L0-b).
//!
//! The registry itself is a host concern: [`ProviderServices`] is the
//! erased hand-off, and the host downcasts to the class it owns. That is the
//! one deliberate type-erasure seam for providers, mirroring
//! `ExtensionService`'s for transports.

use alloc::boxed::Box;
use core::any::Any;

use crate::error::{DepKind, RuntimeError};

/// One provider family: what a driver publishes, keyed the way consumers
/// defer on it.
///
/// Implemented once per family in the crate that owns the family's lookup
/// trait. Adding a family adds a descriptor, not a new registration path.
pub trait ProviderClass: Send + Sync + 'static {
    /// Class-scoped key: the same value consumers put in [`DepSource::id`],
    /// so publishing wakes exactly the parked consumers.
    ///
    /// [`DepSource::id`]: crate::DepSource::id
    type Key: Copy + Into<u64>;

    /// The lookup trait the class registry stores (`dyn ClkLookup`,
    /// `dyn GpioChip`, ...).
    type Lookup: ?Sized + Send + Sync + 'static;

    /// The dependency kind this class satisfies (`DepKind::Clk`, ...).
    const KIND: DepKind;
}

/// One erased provider publication — what a driver hands to
/// [`Device::provide`](crate::Device::provide).
///
/// The payload is the class's `&'static dyn Lookup`, type-erased to `Any`
/// because the device model must not name a family. The owning class
/// recovers it with [`ProviderRegistration::lookup`]; a registration the
/// host does not know is rejected, never silently dropped.
pub struct ProviderRegistration {
    kind: DepKind,
    key: u64,
    lookup: Box<dyn Any + Send + Sync>,
}

impl ProviderRegistration {
    /// Erase a provider into its class. Built by
    /// [`Device::provide`](crate::Device::provide), never by drivers.
    pub fn new<C: ProviderClass>(key: C::Key, lookup: &'static C::Lookup) -> Self {
        Self {
            kind: C::KIND,
            key: key.into(),
            lookup: Box::new(lookup),
        }
    }

    /// The dependency kind this publication satisfies.
    pub const fn kind(&self) -> DepKind {
        self.kind
    }

    /// The class-scoped key (`DepSource::id`).
    pub const fn key(&self) -> u64 {
        self.key
    }

    /// Recover the class lookup. `None` means the registration belongs to a
    /// different class — the host maps that to `RuntimeError::Invalid`.
    pub fn lookup<C: ProviderClass>(self) -> Option<&'static C::Lookup> {
        self.lookup
            .downcast::<&'static C::Lookup>()
            .ok()
            .map(|boxed| *boxed)
    }
}

/// Provider publication, the host-facing capability behind
/// [`Device::provide`](crate::Device::provide).
///
/// Split from the resource capabilities so a host states exactly which
/// provider families it can serve: unsupported classes fail loudly at
/// registration instead of parking consumers forever.
pub trait ProviderServices: Send + Sync {
    /// Publish one provider. The host downcasts to the class it owns and
    /// wakes every consumer parked on `registration.key()`.
    fn register_provider(&self, registration: ProviderRegistration) -> Result<(), RuntimeError>;
}

/// A host with no provider registry (tests, firmware-less boards).
///
/// Registration is refused rather than ignored: a provider driver whose
/// `dev.provide` fails sees a fatal probe error, which is diagnosable;
/// a silently dropped registration parks its consumers with no trace.
impl ProviderServices for () {
    fn register_provider(&self, _registration: ProviderRegistration) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}
