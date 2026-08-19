// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime MMIO mapping backend and device-region registry.

extern crate alloc;

use alloc::vec::Vec;

use khal::paging::MappingFlags;
use kspin::SpinNoIrq;
use memaddr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange};

use crate::{
    kernel_layout,
    kernel_va::{KERNEL_VA_ALLOCATOR, KernelVaReservation, iomap_window},
};

/// Error returned by runtime device IO mapping helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMapError {
    InvalidRange,
    MappingFailed,
    NoMemory,
}

/// A registered device MMIO region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRegion {
    pub paddr: PhysAddr,
    pub size: usize,
    pub name: &'static str,
    pub vaddr: Option<VirtAddr>,
}

/// Iterator over registered device MMIO regions.
pub type DeviceRegionIter = alloc::vec::IntoIter<DeviceRegion>;

/// Lifetime policy of a registered MMIO region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionRefs {
    /// Statically owned region (fixed/boot mapping). Never auto-unmapped.
    Pinned,
    /// Runtime `iomap_device` mapping with an active reference count.
    Counted(usize),
    /// Last reference is being torn down.  Keep the registry entry visible so
    /// concurrent callers cannot acquire a mapping whose PTEs are being
    /// removed or publish a duplicate entry for the same physical interval.
    Removing,
}

/// Internal registry entry pairing a region with its lifetime policy.
#[derive(Debug, Clone, Copy)]
struct RegionEntry {
    region: DeviceRegion,
    refs: RegionRefs,
    /// Dynamic mappings retain the VA reservation through PTE teardown and
    /// the subsequent TLB shootdown. Pinned boot mappings have no teardown.
    reservation: Option<KernelVaReservation>,
}

static DEVICE_REGISTRY: SpinNoIrq<Vec<RegionEntry>> = SpinNoIrq::new(Vec::new());

/// Register a named device MMIO region for diagnostics and memory-region views.
pub fn register_device_region(
    paddr: PhysAddr,
    size: usize,
    name: &'static str,
) -> Result<(), IoMapError> {
    register_region(DeviceRegion {
        paddr,
        size,
        name,
        vaddr: None,
    })
}

/// Register a named device MMIO region that already has a fixed virtual address.
pub fn register_fixed_device_region(
    paddr: PhysAddr,
    size: usize,
    name: &'static str,
    vaddr: VirtAddr,
) -> Result<(), IoMapError> {
    if vaddr.align_offset_4k() != paddr.align_offset_4k() {
        return Err(IoMapError::InvalidRange);
    }
    let start = paddr.align_down_4k();
    let end = paddr
        .checked_add(size)
        .ok_or(IoMapError::InvalidRange)?
        .align_up_4k();
    let span = end.sub_addr(start);
    let va_range = VirtAddrRange::try_from_start_size(vaddr.align_down_4k(), span)
        .ok_or(IoMapError::InvalidRange)?;
    if !iomap_window().contains_range(va_range) {
        return Err(IoMapError::InvalidRange);
    }
    register_region(DeviceRegion {
        paddr,
        size,
        name,
        vaddr: Some(vaddr),
    })
}

/// Return an iterator over runtime-registered device MMIO regions.
pub fn device_regions() -> DeviceRegionIter {
    DEVICE_REGISTRY
        .lock()
        .iter()
        .map(|entry| entry.region)
        .collect::<Vec<_>>()
        .into_iter()
}

/// Map a device MMIO region and register it for diagnostics.
///
/// Drivers are expected to call this during initialization, cache the returned
/// virtual base, and reuse that base from fast paths such as IRQ handlers.
///
/// Each successful call takes one reference on the underlying mapping. Callers
/// that want lifetime-managed teardown should pair this with [`iounmap`].
pub fn iomap_device(
    paddr: PhysAddr,
    size: usize,
    name: &'static str,
) -> Result<VirtAddr, IoMapError> {
    if size == 0 || paddr.checked_add(size).is_none() {
        return Err(IoMapError::InvalidRange);
    }

    let start = paddr.align_down_4k();
    let end = paddr
        .checked_add(size)
        .ok_or(IoMapError::InvalidRange)?
        .align_up_4k();
    let span = end.sub_addr(start);

    if let Some(vaddr) = acquire_mapping_locked(&mut DEVICE_REGISTRY.lock(), start, span)
        .map_err(|_| IoMapError::MappingFailed)?
    {
        return Ok(vaddr + paddr.align_offset_4k());
    }

    kplat::mmio::prepare(start.as_usize(), span).map_err(|_| IoMapError::MappingFailed)?;

    let reservation = KERNEL_VA_ALLOCATOR
        .lock()
        .reserve_in(iomap_window(), span, memaddr::PAGE_SIZE_4K)
        .map_err(|_| IoMapError::NoMemory)?;

    let mut kernel_aspace = kernel_layout().lock();
    let mut regions = DEVICE_REGISTRY.lock();

    if let Some(vaddr) =
        acquire_mapping_locked(&mut regions, start, span).map_err(|_| IoMapError::MappingFailed)?
    {
        drop(regions);
        drop(kernel_aspace);
        KERNEL_VA_ALLOCATOR
            .lock()
            .release(reservation)
            .map_err(|_| IoMapError::MappingFailed)?;
        return Ok(vaddr + paddr.align_offset_4k());
    }

    // Reserve the registry slot before changing the page table.  This keeps
    // the failure path transactional: an allocation failure here cannot leave
    // a live PTE with no corresponding registry entry.
    if regions.try_reserve(1).is_err() {
        drop(regions);
        drop(kernel_aspace);
        KERNEL_VA_ALLOCATOR
            .lock()
            .release(reservation)
            .map_err(|_| IoMapError::MappingFailed)?;
        return Err(IoMapError::NoMemory);
    }

    let mapped_start = reservation.start();
    if kernel_aspace
        .map_linear(
            mapped_start,
            start,
            span,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
        )
        .is_err()
    {
        drop(regions);
        drop(kernel_aspace);
        KERNEL_VA_ALLOCATOR
            .lock()
            .release(reservation)
            .map_err(|_| IoMapError::MappingFailed)?;
        return Err(IoMapError::MappingFailed);
    }
    // A newly allocated interval has no stale TLB entry.  `map_linear`
    // commits with `finish_no_flush()` so this lock can never be held across
    // a synchronous shootdown.

    let region = DeviceRegion {
        paddr: start,
        size: span,
        name,
        vaddr: Some(mapped_start),
    };
    regions.push(RegionEntry {
        region,
        refs: RegionRefs::Counted(1),
        reservation: Some(reservation),
    });
    Ok(mapped_start + paddr.align_offset_4k())
}

/// Release a reference previously taken by [`iomap_device`].
///
/// When the last reference to a runtime mapping is dropped, the virtual mapping
/// is torn down. Pinned (statically registered) regions and addresses that do
/// not belong to any runtime mapping are ignored.
pub fn iounmap(vaddr: VirtAddr) -> Result<(), IoMapError> {
    let mut regions = DEVICE_REGISTRY.lock();
    let Some(index) = regions.iter().position(|entry| {
        let Some(base) = entry.region.vaddr else {
            return false;
        };
        let Some(region_end) = base.checked_add(entry.region.size) else {
            return false;
        };
        matches!(entry.refs, RegionRefs::Counted(_)) && base <= vaddr && vaddr < region_end
    }) else {
        return Ok(());
    };

    let RegionRefs::Counted(count) = regions[index].refs else {
        return Ok(());
    };
    if count > 1 {
        regions[index].refs = RegionRefs::Counted(count - 1);
        return Ok(());
    }

    let entry = regions[index];
    let Some(base) = entry.region.vaddr else {
        return Ok(());
    };
    let reservation = entry
        .reservation
        .expect("counted iomap entry must own a VA reservation");
    regions[index].refs = RegionRefs::Removing;
    drop(regions);

    let unmap_result = {
        let mut layout = kernel_layout().lock();
        layout.unmap_no_release(base, entry.region.size)
    };
    if unmap_result.is_err() {
        // Restore the reference on a failed page-table teardown.  The
        // mapping remains live, so dropping the registry entry would lose
        // ownership of its VA and make a later retry impossible.
        if let Some(current) = DEVICE_REGISTRY
            .lock()
            .iter_mut()
            .find(|candidate| candidate.region == entry.region)
        {
            current.refs = RegionRefs::Counted(1);
        }
        return Err(IoMapError::MappingFailed);
    }

    // The mapping metadata and shared VA reservation remain owned until every
    // CPU has discarded the old translation. This mirrors vmalloc's release
    // ordering and prevents a racing iomap from reusing a stale VA.
    crate::flush_kernel_tlb(None);
    kernel_layout()
        .lock()
        .release_mapping(base, entry.region.size)
        .expect("iomap mapping metadata must match the removed PTEs");
    KERNEL_VA_ALLOCATOR
        .lock()
        .release(reservation)
        .expect("iomap VA reservation must be released exactly once");
    let mut regions = DEVICE_REGISTRY.lock();
    if let Some(index) = regions.iter().position(|candidate| {
        candidate.region == entry.region && matches!(candidate.refs, RegionRefs::Removing)
    }) {
        regions.swap_remove(index);
    }
    Ok(())
}

fn register_region(region: DeviceRegion) -> Result<(), IoMapError> {
    if region.size == 0 || region.paddr.checked_add(region.size).is_none() {
        return Err(IoMapError::InvalidRange);
    }

    let mut regions = DEVICE_REGISTRY.lock();
    if regions.iter().any(|entry| entry.region == region) {
        return Ok(());
    }

    regions.try_reserve(1).map_err(|_| IoMapError::NoMemory)?;
    regions.push(RegionEntry {
        region,
        refs: RegionRefs::Pinned,
        reservation: None,
    });
    Ok(())
}

/// Find a registered mapping that covers `[paddr, paddr + size)` and, when it is
/// a runtime reference-counted mapping, take an additional reference. `Err(())`
/// means that the matching entry is in the `Removing` state; callers must not
/// publish a second mapping until that teardown completes.
fn acquire_mapping_locked(
    regions: &mut [RegionEntry],
    paddr: PhysAddr,
    size: usize,
) -> Result<Option<VirtAddr>, ()> {
    let Some(req_end) = paddr.checked_add(size) else {
        return Ok(None);
    };
    let index = regions.iter().position(|entry| {
        let Some(region_end) = entry.region.paddr.checked_add(entry.region.size) else {
            return false;
        };
        entry.region.vaddr.is_some() && entry.region.paddr <= paddr && region_end >= req_end
    });
    let Some(index) = index else {
        return Ok(None);
    };

    let entry = &mut regions[index];
    let Some(mapped_start) = entry.region.vaddr else {
        return Ok(None);
    };
    match entry.refs {
        RegionRefs::Counted(count) => entry.refs = RegionRefs::Counted(count + 1),
        RegionRefs::Pinned => {}
        // Do not create a second mapping for the same physical interval while
        // the last reference is being torn down.  The caller can retry after
        // the removal completes.
        RegionRefs::Removing => return Err(()),
    }
    Ok(Some(mapped_start + paddr.sub_addr(entry.region.paddr)))
}
