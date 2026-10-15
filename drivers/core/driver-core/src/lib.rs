// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device model spine for porting Linux drivers into x-kernel.
//!
//! This crate is the device model core of the driver framework. It keeps a
//! deliberate structural homomorphism with the Linux driver model so that
//! porting a Linux driver is a mostly mechanical, file-local transformation:
//!
//! | Linux | here |
//! |---|---|
//! | `struct platform_driver` + `probe`/`remove` | [`DriverOps`] with typed [`DriverOps::Data`] |
//! | `struct device` private data (`dev_get_drvdata`) | [`DriverOps::Data`] owned by the framework |
//! | `static const struct of_device_id foo_of_match[]` | `ids: &'static [OfMatch]` on [`DriverDesc`] |
//! | `module_pci_driver()` / `module_init()` | [`DeviceDomain::register_driver`] |
//! | `platform_get_resource(pdev, IORESOURCE_MEM, n)` | [`DeviceCtx::mmio`] / [`DeviceCtx::mmio_by_name`] |
//! | `platform_get_irq_byname` | [`DeviceCtx::irq_by_name`] |
//! | `devm_ioremap_resource` | [`DeviceCtx::iomap`] returning an owned [`IoMap`] guard |
//! | `devm_request_irq` | [`DeviceCtx::request_irq`] returning an owned [`DevIrq`] guard |
//! | `dma_alloc_coherent` / `dmam_alloc_coherent` | [`DeviceCtx::alloc_coherent`] |
//! | `clk_get(dev, "apb_pclk")` | [`DeviceCtx::clk`] |
//! | `-EPROBE_DEFER` | [`ProbeError::Deferred`] |
//!
//! # Ownership model
//!
//! Every resource acquired through [`DeviceCtx`] is returned as a RAII guard.
//! The intended discipline is: **store all guards in [`DriverOps::Data`]**.
//! Probe failure then means "drop `Data`", which releases every guard in
//! field-declaration order (Rust drops fields first-to-last); device removal
//! means "quiesce hardware in `remove()`, then drop `Data`". Arrange field
//! order so dependents release before their dependencies (e.g. the IRQ guard
//! field before the MMIO mapping it serves). RAII is not a substitute for
//! explicit hardware quiesce — `remove()` does that.
//!
//! # Deferred probe
//!
//! A probe that is missing a dependency (for example a clock provider that has
//! not registered yet) returns [`ProbeError::Deferred`] with the dependency it
//! waits for. The domain parks the device and re-probes it exactly when that
//! dependency is published ([`DeviceDomain::wake`]) — no polling loops.
//!
//! # Extension points
//!
//! * New buses implement [`Bus`] in their own crate; nothing in this crate
//!   enumerates PCI or platform devices.
//! * New device services (regulators, pinctrl, ...) plug into
//!   [`DeviceServices`] the same way the clock service does.
//!
//! # Architectural invariants
//!
//! Two rules are load-bearing for this crate specifically. They are stated
//! here, not only in `docs/ai/driver-subsystem-boundaries.md`, because they
//! are the reason the crate has the shape it has.
//!
//! **L0-a — this crate names no device family.** No `virtio`, `nvme`, `ahci`,
//! `mmc`, `i2c` or `usb` may appear in this crate's API, and no family-specific
//! method may be added to [`DeviceCtx`] or [`DeviceServices`]. This is a
//! *fan-in* rule rather than a portability rule: every driver in the tree
//! shares this crate and its test suite, so a family-specific addition here is
//! a change that risks all of them. A family that needs host machinery beyond
//! the capability traits declares an [`ExtensionService`] in its own crate and
//! recovers it through [`DeviceCtx::extension`] — the extension names the host
//! type it supports, this crate never does.
//!
//! Check: `grep -rni 'virtio|nvme|ahci|mmc|i2c|usb' src/` must return
//! comments only.
//!
//! **L0-b — the contracts here are host-free.** [`DeviceResourceServices`],
//! [`ClockConsumerServices`] and [`DeviceServices`] must not name a
//! host-kernel type in their signatures, because that is what lets the model
//! be exercised against a mock (`domain/tests.rs` runs 19 state-machine tests
//! on `MockServices`). The one deliberate exception is the IRQ *vocabulary*
//! re-exported by [`crate::resource`] (`IrqTrigger`, `IrqController`,
//! `IrqDomainId`): those shapes are defined exactly once in the tree, in
//! `kirq`, and are data rather than host behaviour.
//!
//! Providers — the [`provider`] Op traits — are the host-facing surface and
//! are *not* covered by L0-b; `driver-host` implements them. Drivers do not
//! touch them (rule L1: one acquisition path, [`DeviceCtx`]).

#![no_std]

extern crate alloc;

pub mod bus;
mod device;
mod domain;
mod driver;
mod error;
pub mod provider;
mod resource;

pub mod desc;
mod handle;
mod services;

pub use bus::{Bus, BusDeviceData, BusId, BusInstanceId, MatchScore};
pub use desc::DeviceDescriptor;
pub use device::{BoundDevice, DeviceCtx, DeviceId};
pub use domain::{
    DataBox, DeviceDomain, DeviceSnapshot, DeviceState, ProbeStats, global_domain,
    global_topology_summary,
};
pub use driver::{DriverDesc, DriverOps};
pub use error::{DepKind, DepSource, ProbeError, RuntimeError};
pub use handle::{
    ClkHandle, CoherentMem, DevIrq, EnabledClock, IoMap, IrqAction, IrqHandlerFn, ReleaseHook,
};
pub use provider::ClkOps;
pub use resource::{
    ClkSpec, DmaInfo, IoPortRange, IrqController, IrqDomainId, IrqSpec, IrqTrigger, MmioRegion,
    ResourceEntry, ResourceKind, ResourceMap,
};
pub use services::{
    ClockConsumerServices, DeviceResourceServices, DeviceServices, ExtensionService, ServiceError,
};

pub mod platform {
    //! The platform bus's public vocabulary (moved to
    //! [`crate::bus::platform_bus`]); re-exported for driver-side imports.

    pub use crate::bus::platform_bus::{
        AliasMatch, FirmwareNodeInfo, FirmwareProp, OfMatch, PlatformBus, PlatformBusData,
        PlatformBusIdentity, PlatformId,
    };
}
