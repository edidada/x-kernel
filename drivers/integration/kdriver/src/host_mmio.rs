// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One owned host mapping reference for each model MMIO guard.

use driver_model::{DeviceContext, IoMap, MmioRegion, RuntimeError};

/// Map a bus-resolved device region for the current model callback.
///
/// This host helper implements the `DeviceResourceServices::iomap` operation.
/// Like `DeviceAccess::res().iomap_region`, it accepts explicit bus-resolved
/// ranges, not only descriptor-indexed resources. The trusted bus/driver owns
/// resource selection; this is not a physical-address authorization mechanism.
/// Each returned guard owns one memspace mapping reference. Drop releases that
/// reference; another guard or a pinned boot mapping may keep the mapping live.
/// Callers must stop all device/register access before dropping their guard.
///
/// # Errors
/// Invalid/empty ranges return `Invalid`, address-space or registry allocation
/// failure returns `NoMemory`, and platform/page-table failures return `Io`.
/// Release cannot return an error from Drop; failures are logged, not retried.
/// The guard retains the provider directly, without allocating a release hook.
pub fn iomap(
    _device: DeviceContext<'_>,
    region: MmioRegion,
    name: &'static str,
) -> Result<IoMap, RuntimeError> {
    map_region(region, name)
}

fn map_region(region: MmioRegion, name: &'static str) -> Result<IoMap, RuntimeError> {
    IoMap::map_with(crate::resource::resource_provider(), region, name).map_err(RuntimeError::from)
}

unittest::path_mod_test!(tests, "../ut-tests/host_mmio.rs");
