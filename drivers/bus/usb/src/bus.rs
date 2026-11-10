// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! What the bus knows about the devices on it, and who has claimed them.
//!
//! Enumeration produces [`UsbDevice`]s inside a controller's probe, and a class
//! driver turns one of them into a published device (a block device, an input
//! device, …). Both facts have to outlive probe, because devices are removed and
//! re-appear at runtime; keeping them in the controller's probe data left nothing
//! able to withdraw a publication or to publish a re-enumerated device.
//!
//! Two rules shape this module:
//!
//! - **No class types.** `bus/usb` must not depend on `subsys_block`/`subsys_input`
//!   /… , so a binding's withdrawal owners are stored type-erased as
//!   `Box<dyn Any + Send + Sync>`. Dropping the box runs the class's withdrawal.
//! - **Removal returns ownership, it does not drop.** A `Publication` tends to hold
//!   the controller (the block backend does, through `Arc<Mutex<Hcd>>`), so dropping
//!   one while the controller lock is held would wait for a consumer that needs that
//!   same lock. [`UsbBus::detach`] therefore hands the bindings back to the caller,
//!   which drops them after releasing the controller.
//!
//! Lock order: `UsbBus` is always taken *before* the controller's lock.

use alloc::{boxed::Box, vec::Vec};
use core::any::Any;

use kerrno::{KError, KResult};

use crate::UsbDevice;

/// One class binding on one device.
///
/// The owners are the values whose `Drop` withdraws the published device; they are
/// type-erased so this module stays class-agnostic.
pub struct ClassBinding {
    /// The class that claimed the device (`"usb-storage"`, …), for logs and for
    /// reporting which binding a removal took down.
    pub class_name: &'static str,
    /// The interface this binding covers.
    pub interface: u8,
    /// Withdrawal owners: dropping these unpublishes.
    pub owners: Vec<Box<dyn Any + Send + Sync>>,
}

impl ClassBinding {
    pub fn new(
        class_name: &'static str,
        interface: u8,
        owners: Vec<Box<dyn Any + Send + Sync>>,
    ) -> Self {
        Self {
            class_name,
            interface,
            owners,
        }
    }
}

impl core::fmt::Debug for ClassBinding {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The owners' types are erased on purpose; the names are the report.
        formatter
            .debug_struct("ClassBinding")
            .field("class_name", &self.class_name)
            .field("interface", &self.interface)
            .field("owners", &self.owners.len())
            .finish()
    }
}

/// One device on the bus and everything that was published for it.
pub struct UsbDeviceEntry {
    device: UsbDevice,
    bindings: Vec<ClassBinding>,
}

impl UsbDeviceEntry {
    pub fn device(&self) -> &UsbDevice {
        &self.device
    }

    /// The class that claimed `interface`, if any.
    pub fn binding(&self, interface: u8) -> Option<&ClassBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.interface == interface)
    }

    pub fn bindings(&self) -> impl Iterator<Item = &ClassBinding> {
        self.bindings.iter()
    }
}

/// The devices on one controller and their class bindings.
///
/// Owned by the controller driver (through an `Arc<Mutex<..>>` handed in at probe),
/// so a port scan at runtime can add and remove entries long after probe returned.
pub struct UsbBus {
    /// Identity the bindings publish under. Obtained in probe from the controller's
    /// own `Device`, and usable afterwards -- see `driver_base::DeviceHandle`.
    handle: driver_base::DeviceHandle,
    devices: Vec<UsbDeviceEntry>,
}

impl UsbBus {
    pub fn new(handle: driver_base::DeviceHandle) -> Self {
        Self {
            handle,
            devices: Vec::new(),
        }
    }

    /// The identity class bindings are published under.
    pub fn handle(&self) -> &driver_base::DeviceHandle {
        &self.handle
    }

    pub fn devices(&self) -> impl Iterator<Item = &UsbDeviceEntry> {
        self.devices.iter()
    }

    /// The device registered at `address`, if any.
    pub fn device(&self, address: u8) -> Option<&UsbDevice> {
        self.devices
            .iter()
            .find(|entry| entry.device.address == address)
            .map(|entry| &entry.device)
    }

    pub fn contains(&self, address: u8) -> bool {
        self.device(address).is_some()
    }

    /// Records a freshly enumerated device.
    ///
    /// Two devices cannot share an address: a second `attach` for the same address
    /// means the caller forgot to detach the previous one, and silently replacing it
    /// would leak that device's bindings.
    pub fn attach(&mut self, device: UsbDevice) -> KResult<()> {
        if self.contains(device.address) {
            log::error!(
                "usb: refusing to attach a second device at address {}",
                device.address
            );
            return Err(KError::BadState);
        }
        log::info!(
            "usb: address {} attached ({})",
            device.address,
            device.summary()
        );
        self.devices.push(UsbDeviceEntry {
            device,
            bindings: Vec::new(),
        });
        Ok(())
    }

    /// Adds one class binding. All-or-nothing: a failed bind leaves no entry.
    ///
    /// The failure carries the binding back out. Dropping it here would run a
    /// withdrawal under this lock, which is the deadlock [`Self::detach`] exists to
    /// avoid; the caller decides where the owners are released.
    pub fn bind(
        &mut self,
        address: u8,
        binding: ClassBinding,
    ) -> core::result::Result<(), RejectedBinding> {
        let Some(entry) = self
            .devices
            .iter_mut()
            .find(|entry| entry.device.address == address)
        else {
            log::error!("usb: bind for unknown address {address}");
            return Err(RejectedBinding {
                error: KError::NotFound,
                binding,
            });
        };
        if entry.binding(binding.interface).is_some() {
            log::error!(
                "usb: address {address} interface {} is already bound to `{}`",
                binding.interface,
                entry
                    .binding(binding.interface)
                    .map_or("", |b| b.class_name)
            );
            return Err(RejectedBinding {
                error: KError::BadState,
                binding,
            });
        }
        log::info!(
            "usb: address {address} interface {} bound to `{}`",
            binding.interface,
            binding.class_name
        );
        entry.bindings.push(binding);
        Ok(())
    }

    /// Removes a device and **returns** its bindings.
    ///
    /// The caller drops the returned bindings after releasing the controller lock:
    /// a withdrawal may wait for an in-flight consumer, and that consumer needs the
    /// controller. Dropping them here, under the caller's controller lock, is the
    /// deadlock this signature exists to prevent.
    pub fn detach(&mut self, address: u8) -> Vec<ClassBinding> {
        let Some(index) = self
            .devices
            .iter()
            .position(|entry| entry.device.address == address)
        else {
            return Vec::new();
        };
        let entry = self.devices.remove(index);
        log::info!(
            "usb: address {address} detached ({} binding(s) to withdraw)",
            entry.bindings.len()
        );
        entry.bindings
    }

    /// Removes every device, returning all bindings (controller reset / removal).
    pub fn detach_all(&mut self) -> Vec<ClassBinding> {
        let mut bindings = Vec::new();
        while let Some(entry) = self.devices.pop() {
            bindings.extend(entry.bindings);
        }
        bindings
    }
}

/// A binding the table refused, handed back with the reason.
///
/// Splitting the error from the owners keeps the release point explicit: the
/// caller drops `binding` after it has released the lock it held.
pub struct RejectedBinding {
    pub error: KError,
    pub binding: ClassBinding,
}

/// Drops bindings that a removal handed back.
///
/// A free function rather than a method so the release point is visibly *outside*
/// any `UsbBus` or controller critical section: `drop_bindings(bindings)` reads as
/// "we are no longer holding anything".
pub fn drop_bindings(bindings: Vec<ClassBinding>) {
    for binding in bindings {
        log::info!(
            "usb: withdrawing `{}` on interface {}",
            binding.class_name,
            binding.interface
        );
        drop(binding);
    }
}
