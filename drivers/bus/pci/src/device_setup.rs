// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared PCI helper routines used by the compose enumeration and driver
//! activation paths.

use kerrno::{KError, KResult};
use kspin::{SpinNoPreempt, SpinNoPreemptGuard};
use lazyinit::LazyInit;

/// Lazily-initialised allocator for unassigned PCI memory BARs.
///
/// `LazyInit::call_once` runs the constructor *outside* of any spinlock
/// guard, so it is safe for the constructor to walk firmware tables (which
/// may take their own locks). BAR assignment only happens during driver
/// probe / bus enumeration in process context, so a [`SpinNoPreempt`] is
/// sufficient to serialise access to the shared allocation cursor.
static PCI_BAR_ALLOCATOR: LazyInit<SpinNoPreempt<Option<crate::PciRangeAllocator>>> =
    LazyInit::new();

fn pci_bar_allocator() -> SpinNoPreemptGuard<'static, Option<crate::PciRangeAllocator>> {
    PCI_BAR_ALLOCATOR.call_once(|| {
        let alloc = crate::pci_bar_allocation_range()
            .map(|(start, len, _)| crate::PciRangeAllocator::new(start, len));
        SpinNoPreempt::new(alloc)
    });
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed immediately above"
    )]
    {
        PCI_BAR_ALLOCATOR
            .get()
            .expect("PCI_BAR_ALLOCATOR call_once succeeded")
            .lock()
    }
}

pub fn pci_cam_kind() -> crate::Cam {
    #[cfg(feature = "pci-non-ecam")]
    {
        crate::Cam::MmioCam
    }

    #[cfg(not(feature = "pci-non-ecam"))]
    {
        crate::Cam::Ecam
    }
}

/// Whether any memory BAR of `bdf` must be assigned before the device is usable.
///
/// "Must be assigned" is wider than "reads zero": U-Boot runs its own PCI
/// enumeration before the kernel starts and programmes BARs in its address map,
/// which can land in DRAM. A non-zero BAR outside `window` (this host's MMIO
/// window) is just as unusable, so it is reported here and rewritten by
/// [`crate::configure_device`].
fn has_memory_bar_needing_assignment<C: crate::ConfigurationAccess>(
    root: &mut crate::PciRoot<C>,
    bdf: crate::DeviceFunction,
    window: Option<(u64, u64)>,
) -> bool {
    let mut bar_idx = 0u8;
    while bar_idx < 6 {
        let info = match root.bar_info(bdf, bar_idx) {
            Ok(Some(info)) => info,
            _ => {
                bar_idx += 1;
                continue;
            }
        };

        let advance = if info.takes_two_entries() { 2 } else { 1 };
        if crate::memory_bar_needs_assignment(&info, window) {
            return true;
        }

        bar_idx += advance;
    }

    false
}

/// Allocate unassigned PCI memory BARs if needed, then enable runtime access.
///
/// If any memory BAR is still unassigned, this function allocates an address
/// from the shared PCI MMIO window and writes it back into config space. It
/// then enables the standard PCI command bits (`IO_SPACE`, `MEMORY_SPACE`,
/// `BUS_MASTER`) expected by runtime drivers.
///
/// If all memory BARs are already assigned, the shared allocator is not
/// touched; the call only refreshes the device command bits. If any memory BAR
/// is unassigned and no allocation range is available, returns
/// [`KError::NoMemory`].
pub fn configure_pci_device_if_needed<C: crate::ConfigurationAccess>(
    root: &mut crate::PciRoot<C>,
    bdf: crate::DeviceFunction,
) -> KResult {
    let window = pci_bar_allocator()
        .as_ref()
        .map(crate::PciRangeAllocator::window);
    if !has_memory_bar_needing_assignment(root, bdf, window) {
        let mut no_allocator = None;
        return crate::configure_device(root, bdf, &mut no_allocator).map_err(|err| {
            warn!("failed to configure PCI device {:?}: {:?}", bdf, err);
            match err {
                crate::PciInitError::NoMemory => KError::NoMemory,
                crate::PciInitError::InvalidRange => KError::InvalidInput,
                crate::PciInitError::MappingFailed => KError::Io,
            }
        });
    }

    let mut allocator = pci_bar_allocator();
    if allocator.is_none() {
        warn!(
            "PCI device {:?} needs memory BAR assignment but no allocation range is known",
            bdf
        );
        return Err(KError::NoMemory);
    }

    crate::configure_device(root, bdf, &mut allocator).map_err(|err| {
        warn!("failed to configure PCI device {:?}: {:?}", bdf, err);
        match err {
            crate::PciInitError::NoMemory => KError::NoMemory,
            crate::PciInitError::InvalidRange => KError::InvalidInput,
            crate::PciInitError::MappingFailed => KError::Io,
        }
    })
}
