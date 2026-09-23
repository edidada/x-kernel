// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device model spine for porting Linux drivers into x-kernel.
//!
//! This crate is the Phase 0 core of the driver framework refactor. It keeps a
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
//! Probe failure then means "drop `Data`", which rolls back every resource in
//! reverse field order; device removal means "quiesce in `remove()`, then drop
//! `Data`". There is no separate devres closure list and no driver-side global
//! cleanup tables.
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

#![no_std]

extern crate alloc;

mod bus;
mod device;
mod domain;
mod driver;
mod error;
mod resource;

pub use bus::{Bus, BusId, IdMatchEntry, MatchScore};
pub use device::{
    BoundDevice, ClkHandle, ClkOps, CoherentMem, DevIrq, DeviceCtx, DeviceServices, IoMap,
    IrqAction, IrqHandlerFn, ReleaseHook, ServiceError,
};
pub use domain::{DataBox, DeviceDomain, DeviceSnapshot, DeviceState, ProbeStats, global_domain};
pub use driver::{DriverDesc, DriverOps};
pub use error::{DepKind, DepSource, ProbeError, RuntimeError};
pub use resource::{
    ClkSpec, IoPortRange, IrqSpec, IrqTrigger, MmioRegion, ResourceEntry, ResourceKind, ResourceMap,
};

pub mod desc {
    //! Enumeration products handed to the model by bus backends.

    pub use crate::bus::{
        DeviceDescriptor, DeviceIdentity, DeviceLocation, DiscoveryOrigin, FirmwareNodeInfo,
        PciIdentity, PlatformIdentity,
    };
}

pub mod platform {
    //! Reference implementation of the platform ("firmware node") bus.
    //!
    //! Phase 3 of the refactor moves this into a dedicated `bus-platform`
    //! crate; it lives here so the Phase 0 acceptance work (porting the PL011
    //! serial driver from the `clk/tail` branch) can proceed without waiting
    //! for the bus crates.

    pub use crate::bus::{OfMatch, PlatformBus};
}
