// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hardware RNG registration and operation-only consumer handles.

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_class::{Class, Publication, Registration};
use kerrno::KResult;

use super::HwrngDevice;

/// Driver-supplied runtime; never exposed by consumer handles.
pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;

/// Hardware RNG registry with a subsystem-specific consumer interface.
pub struct HwrngClass {
    class: Class<HwrngDeviceImpl>,
}

impl HwrngClass {
    /// Construct an empty registry for static initialization.
    pub const fn new() -> Self {
        Self {
            class: Class::new(()),
        }
    }

    /// Transitional registration for drivers still using the legacy model.
    #[doc(hidden)]
    pub fn attach_legacy(
        &'static self,
        driver_name: &'static str,
        path: String,
        runtime: HwrngDeviceImpl,
    ) -> KResult<HwrngRegistration> {
        self.class
            .attach_legacy(driver_name, path, runtime)
            .map(|registration| HwrngRegistration { registration })
    }

    /// Snapshot currently available RNG capabilities.
    pub fn devices(&self) -> Vec<HwrngPublication> {
        self.class
            .devices()
            .into_iter()
            .map(|publication| HwrngPublication { publication })
            .collect()
    }

    /// Observe future publications; callbacks run outside registry locks.
    pub fn subscribe_available(&self, callback: Arc<dyn Fn(HwrngPublication) + Send + Sync>) {
        self.class.subscribe_available(Arc::new(move |publication| {
            callback(HwrngPublication { publication });
        }));
    }

    /// Observe withdrawn capabilities without exposing their raw runtimes.
    pub fn subscribe_removed(&self, callback: Arc<dyn Fn(HwrngPublication) + Send + Sync>) {
        self.class.subscribe_removed(Arc::new(move |publication| {
            callback(HwrngPublication { publication });
        }));
    }
}

impl Default for HwrngClass {
    fn default() -> Self {
        Self::new()
    }
}

/// System-wide hardware RNG capability registry.
pub static HWRNG: HwrngClass = HwrngClass::new();

/// Unique RNG registration credential, retained by the publisher.
#[derive(Debug)]
#[must_use = "dropping the registration withdraws the RNG capability"]
pub struct HwrngRegistration {
    registration: Registration<HwrngDeviceImpl>,
}

impl HwrngRegistration {
    /// Obtain an operation-only consumer handle without transferring ownership.
    pub fn publication(&self) -> HwrngPublication {
        HwrngPublication {
            publication: self.registration.publication(),
        }
    }
}

/// Cloneable RNG consumer reference, with no access to the raw driver object.
#[derive(Clone, Debug)]
pub struct HwrngPublication {
    publication: Publication<HwrngDeviceImpl>,
}

impl HwrngPublication {
    /// Name of the driver supplying this capability.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.publication.driver_name()
    }

    /// Snapshot of whether new operations can be admitted.
    pub fn is_available(&self) -> bool {
        self.publication.is_available()
    }

    /// Whether two handles identify the same publication instance.
    pub fn same_publication(&self, other: &Self) -> bool {
        self.publication.same_publication(&other.publication)
    }

    /// Read under an operation permit in sleepable task context.
    ///
    /// Returns BadState after withdrawal; otherwise follows HwrngDevice::read.
    pub fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        self.publication.with_available(|device| device.read(buf))
    }

    /// Query terminal hardware failure under an operation permit.
    ///
    /// Returns BadState after withdrawal rather than querying torn-down state.
    /// Withdrawal is not itself a terminal hardware failure.
    pub fn is_failed(&self) -> KResult<bool> {
        self.publication
            .with_available(|device| Ok(device.is_failed()))
    }
}
