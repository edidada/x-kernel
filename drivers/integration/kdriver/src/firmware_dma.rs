// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Convert resolved firmware DMA facts into portable device configuration.
//!
//! Firmware interpretation stays below this host integration module. Device
//! masks come from device capability, never from the size of a firmware window.

use driver_model::{DmaInfo, DmaWindow};
use khal::firmware::devices::{DmaResolveError, FirmwareDevice, FirmwareDeviceView};

/// Why enumeration cannot yet configure a device's DMA view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaConfigurationError {
    /// The firmware path itself was rejected; no fallback may erase this error.
    Firmware(DmaResolveError),
    /// No explicit window; a separately justified platform policy is required.
    MissingWindow,
    /// No explicit cache-coherency declaration.
    MissingCoherency,
    /// The device mask is not a contiguous set of low address bits.
    InvalidMask,
    /// The supplied resolved window is empty or overflows either address space.
    InvalidWindow,
    /// The node is not an available direct QEMU Arm virt MMIO device.
    UnsupportedPlatformPolicy,
    /// Firmware does not describe one unambiguous nonempty root RAM interval.
    MissingPlatformRam,
}

/// Build configuration for a descriptor from a firmware discovery result.
///
/// The caller supplies the device's address mask. This allocation-free function
/// neither assumes cache coherency nor defaults missing ranges to identity. It
/// preserves the entire bus window independently of the mask; the allocator must
/// subsequently validate every buffer with `DmaInfo::translate_range`.
///
/// # Errors
/// Returns the firmware error unchanged or identifies missing/invalid facts.
/// Platform-specific fallback is intentionally not part of this conversion.
pub fn from_firmware(device: &FirmwareDevice, mask: u64) -> Result<DmaInfo, DmaConfigurationError> {
    let facts = device.dma.map_err(DmaConfigurationError::Firmware)?;
    if mask != u64::MAX && mask & (mask + 1) != 0 {
        return Err(DmaConfigurationError::InvalidMask);
    }
    let range = facts.range.ok_or(DmaConfigurationError::MissingWindow)?;
    let coherent = facts
        .coherent
        .ok_or(DmaConfigurationError::MissingCoherency)?;
    let window = DmaWindow::new(range.cpu_addr, range.bus_addr, range.size)
        .map_err(|_| DmaConfigurationError::InvalidWindow)?;
    Ok(DmaInfo {
        mask,
        window,
        coherent,
    })
}

/// Apply the explicit QEMU Arm virt direct-MMIO identity-DMA policy if needed.
///
/// Explicit firmware windows and errors take precedence. Only an available
/// `virtio,mmio` child of `linux,dummy-virt` may substitute the single root RAM
/// interval for missing `dma-ranges`. Coherency and the caller's device mask
/// remain independent requirements. This does not authorize arbitrary DMA or
/// bypass the native allocator's reserved-memory accounting.
///
/// # Errors
/// Returns conversion errors unchanged, or rejects unsupported topology/RAM.
pub fn from_firmware_with_qemu_virt_mmio_policy(
    view: &FirmwareDeviceView<'_>,
    mask: u64,
) -> Result<DmaInfo, DmaConfigurationError> {
    with_platform_window(view.description(), mask, || {
        if !cfg!(target_arch = "aarch64")
            || !view.is_compatible("virtio,mmio")
            || !view.is_root_child_of("linux,dummy-virt")
        {
            return Err(DmaConfigurationError::UnsupportedPlatformPolicy);
        }
        let (start, size) = view
            .single_root_ram_range()
            .ok_or(DmaConfigurationError::MissingPlatformRam)?;
        DmaWindow::new(start as u64, start as u64, size as u64)
            .map_err(|_| DmaConfigurationError::InvalidWindow)
    })
}

fn with_platform_window(
    device: &FirmwareDevice,
    mask: u64,
    platform_window: impl FnOnce() -> Result<DmaWindow, DmaConfigurationError>,
) -> Result<DmaInfo, DmaConfigurationError> {
    match from_firmware(device, mask) {
        Err(DmaConfigurationError::MissingWindow) => {}
        result => return result,
    }
    let window = platform_window()?;
    let coherent = device
        .dma
        .map_err(DmaConfigurationError::Firmware)?
        .coherent
        .ok_or(DmaConfigurationError::MissingCoherency)?;
    Ok(DmaInfo {
        mask,
        window,
        coherent,
    })
}

unittest::path_mod_test!(tests, "../ut-tests/firmware_dma.rs");
