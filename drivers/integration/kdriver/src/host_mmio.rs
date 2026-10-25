// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One owned host mapping reference for each model MMIO guard.

use alloc::sync::Arc;

use driver_model::{DeviceContext, IoMap, MmioRegion, RuntimeError};
use memspace::IoMapError;

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
/// Release-hook metadata follows the kernel's infallible Arc allocation policy.
pub fn iomap(
    _device: DeviceContext<'_>,
    region: MmioRegion,
    name: &'static str,
) -> Result<IoMap, RuntimeError> {
    map_region(region, name)
}

fn map_region(region: MmioRegion, name: &'static str) -> Result<IoMap, RuntimeError> {
    if region.size > isize::MAX as usize {
        return Err(RuntimeError::Invalid);
    }
    let vaddr = memspace::iomap_device(region.base.into(), region.size, name).map_err(map_error)?;
    let release = Arc::new(move || {
        if let Err(error) = memspace::iounmap(vaddr) {
            log::error!("model MMIO release failed at {vaddr:?}: {error:?}");
        }
    });
    // SAFETY: iomap_device acquired one mapping reference covering region.size
    // bytes at vaddr, including its page offset. The private, unshared hook is
    // transferred to one non-Clone IoMap and releases that reference once.
    Ok(unsafe { IoMap::new(vaddr.as_usize(), region, Some(release)) })
}

fn map_error(error: IoMapError) -> RuntimeError {
    match error {
        IoMapError::InvalidRange => RuntimeError::Invalid,
        IoMapError::NoMemory => RuntimeError::NoMemory,
        IoMapError::MappingFailed => RuntimeError::Io,
    }
}

unittest::path_mod_test!(tests, "../ut-tests/host_mmio.rs");
