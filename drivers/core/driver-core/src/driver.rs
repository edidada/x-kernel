// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver contracts: id tables plus probe/remove with typed private data.

use crate::{
    bus::Bus,
    device::{BoundDevice, DeviceCtx},
    error::ProbeError,
};

/// Driver operations with a typed private-data association.
///
/// `Data` is the Linux `struct foo_priv`: allocated in probe, owned by the
/// framework for the lifetime of the binding, passed back mutably in
/// `remove`, and dropped (releasing every guard it holds) after `remove`
/// returns.
pub trait DriverOps<B: Bus>: Send + Sync + 'static {
    /// Driver-private data; must be `Send + Sync` because interrupt handlers
    /// and work items share it through `Weak` handles.
    type Data: Send + Sync + 'static;

    /// Initialize the device. Store every acquired guard in the returned
    /// `Data`; returning `Err` drops nothing but locals, so partial
    /// initialization rolls back through ordinary RAII.
    fn probe(&self, dev: &mut DeviceCtx<'_, B>) -> Result<Self::Data, ProbeError>;

    /// Quiesce the device. Guards inside `Data` are still alive here and are
    /// released after this returns. The default does nothing.
    fn remove(&self, dev: &mut BoundDevice<'_, B, Self::Data>) {
        let _ = dev;
    }
}

/// A driver registration record: everything is data except the ops.
///
/// The direct translation target of Linux's `struct platform_driver`:
/// `driver.name`, the `*_device_id` tables, and the function pointers.
pub struct DriverDesc<B: Bus, P: DriverOps<B>> {
    pub name: &'static str,
    /// Const id table (`of_device_id` / `pci_device_id` rows).
    pub ids: &'static [B::IdEntry],
    pub ops: P,
}

impl<B: Bus, P: DriverOps<B>> DriverDesc<B, P> {
    pub const fn new(name: &'static str, ids: &'static [B::IdEntry], ops: P) -> Self {
        Self { name, ids, ops }
    }
}
