// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory-block online/offline policy.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};

use kaddr_layout::LINEAR_MAP_VSIZE;
use kdevice::{
    SystemClass, SystemDeviceControl, SystemDeviceDescriptor, SystemDeviceId, SystemDeviceState,
    install_system_device, present_system_devices, set_system_device_online, system_device_state,
};
use kerrno::{KErrorKind, KResult};
use khal::{
    mem::{MemFlags, PAGE_SIZE_4K, PhysAddr, VirtAddr},
    paging::MappingFlags,
};
use kspin::SpinNoPreempt;

use crate::{HotplugController, allocator_error};

/// Size of one memory hotplug block: 128 MiB, matching the x86-64 Linux block
/// size used by the QEMU memory-hotplug ABI.
pub const MEMORY_BLOCK_SIZE_BYTES: u64 = 128 * 1024 * 1024;

/// Platform operations needed to make a memory block online or offline.
///
/// Split from the controller so the state machine is testable without a real
/// direct map or allocator.
pub(crate) trait MemoryBackend: Send + Sync {
    /// Ensures the linear direct map covers the range. Returns whether this call
    /// installed a new mapping, so a later failure can be rolled back.
    fn ensure_direct_mapped(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<bool>;

    /// Removes a direct mapping installed by [`Self::ensure_direct_mapped`].
    fn unmap_direct(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<()>;

    /// Adds the range to the global page allocator.
    fn add_to_allocator(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<()>;

    /// Removes the range from the global page allocator. Every page must be
    /// free; otherwise this returns `ResourceBusy` and leaves the allocator
    /// unchanged.
    fn remove_from_allocator(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<()>;
}

struct KernelMemoryBackend;

static KERNEL_BACKEND: KernelMemoryBackend = KernelMemoryBackend;

/// Memory hotplug policy.
pub struct MemoryHotplugController {
    /// Serializes the range checks with the generic `kdevice` transition so two
    /// requests cannot both observe the same old state.
    transition_lock: SpinNoPreempt<()>,
}

impl MemoryHotplugController {
    /// Creates the production controller.
    pub const fn new() -> Self {
        Self {
            transition_lock: SpinNoPreempt::new(()),
        }
    }
}

impl Default for MemoryHotplugController {
    fn default() -> Self {
        Self::new()
    }
}

impl HotplugController for MemoryHotplugController {
    fn class(&self) -> SystemClass {
        SystemClass::Memory
    }

    fn present(&self) -> Vec<SystemDeviceId> {
        present_system_devices(SystemClass::Memory)
    }

    fn set_online(&self, id: SystemDeviceId, is_online: bool) -> KResult<()> {
        if id.class() != SystemClass::Memory {
            return Err(KErrorKind::InvalidInput.into());
        }
        let _transition = self.transition_lock.lock();
        let state = system_device_state(id).ok_or(KErrorKind::NoSuchDevice)?;
        if state.is_online == is_online {
            return Ok(());
        }

        if is_online {
            validate_memory_block(id, state.physical_start_bytes, state.size_bytes)?;
            validate_block_does_not_overlap(id, state.physical_start_bytes, state.size_bytes)?;
            validate_range_is_free(state.physical_start_bytes, state.size_bytes)?;
        }

        // `MemoryControl` rolls back its own partial direct-map and allocator
        // work before returning an error; the generic layer restores the
        // published state. There is nothing left for the controller to undo.
        set_system_device_online(id, is_online)
    }

    fn state(&self, id: SystemDeviceId) -> Option<SystemDeviceState> {
        if id.class() != SystemClass::Memory {
            return None;
        }
        system_device_state(id)
    }
}

/// Per-device control installed in `kdevice`.
pub(crate) struct MemoryControl {
    id: SystemDeviceId,
    backend: &'static dyn MemoryBackend,
}

impl MemoryControl {
    pub(crate) fn new(id: SystemDeviceId) -> Self {
        Self {
            id,
            backend: &KERNEL_BACKEND,
        }
    }

    #[cfg(unittest)]
    pub(crate) fn with_backend(id: SystemDeviceId, backend: &'static dyn MemoryBackend) -> Self {
        Self { id, backend }
    }
}

impl SystemDeviceControl for MemoryControl {
    fn set_online(&self, is_online: bool) -> KResult<()> {
        let state = system_device_state(self.id).ok_or(KErrorKind::NoSuchDevice)?;
        if state.is_online == is_online {
            return Ok(());
        }

        if is_online {
            validate_memory_block(self.id, state.physical_start_bytes, state.size_bytes)?;
            let mapped_here = self
                .backend
                .ensure_direct_mapped(state.physical_start_bytes, state.size_bytes)?;
            if let Err(error) = self
                .backend
                .add_to_allocator(state.physical_start_bytes, state.size_bytes)
            {
                if mapped_here
                    && self
                        .backend
                        .unmap_direct(state.physical_start_bytes, state.size_bytes)
                        .is_err()
                {
                    log::warn!(
                        "hotplug: failed to roll back direct map for memory device {}",
                        self.id.index()
                    );
                }
                return Err(error);
            }
        } else {
            self.backend
                .remove_from_allocator(state.physical_start_bytes, state.size_bytes)?;
            // The direct map is deliberately kept: removing it would make the
            // physical range inaccessible to later diagnostics, and the
            // allocator no longer hands out its pages. See docs/design.md.
        }
        Ok(())
    }
}

impl MemoryBackend for KernelMemoryBackend {
    fn ensure_direct_mapped(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<bool> {
        validate_memory_range(physical_start_bytes, size_bytes)?;
        let virtual_start = direct_map_virtual_start(physical_start_bytes)?;
        let size = size_bytes as usize;
        let start = VirtAddr::from_usize(virtual_start);

        {
            let layout = memspace::kernel_layout().lock();
            let mut cursor = virtual_start;
            let end = virtual_start
                .checked_add(size)
                .ok_or(KErrorKind::InvalidInput)?;
            let mut is_fully_mapped = true;
            while cursor < end {
                match layout.find_vma(VirtAddr::from_usize(cursor)) {
                    Some(vma)
                        if vma.start().as_usize() <= cursor && vma.end().as_usize() > cursor =>
                    {
                        cursor = vma.end().as_usize();
                    }
                    Some(_) => {
                        is_fully_mapped = false;
                        break;
                    }
                    None => {
                        is_fully_mapped = false;
                        break;
                    }
                }
            }
            if is_fully_mapped {
                return Ok(false);
            }
            if cursor != virtual_start {
                // A partial mapping cannot be extended safely here: the caller
                // has no way to describe which sub-range was already mapped.
                return Err(KErrorKind::ResourceBusy.into());
            }
        }

        memspace::kernel_layout().lock().map_linear(
            start,
            PhysAddr::from_usize(physical_start_bytes as usize),
            size,
            MappingFlags::READ | MappingFlags::WRITE,
        )?;
        Ok(true)
    }

    fn unmap_direct(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<()> {
        let virtual_start = direct_map_virtual_start(physical_start_bytes)?;
        memspace::kernel_layout()
            .lock()
            .unmap(VirtAddr::from_usize(virtual_start), size_bytes as usize)
    }

    fn add_to_allocator(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<()> {
        let virtual_start = direct_map_virtual_start(physical_start_bytes)?;
        kalloc::global_add_memory(virtual_start, size_bytes as usize).map_err(allocator_error)
    }

    fn remove_from_allocator(&self, physical_start_bytes: u64, size_bytes: u64) -> KResult<()> {
        let virtual_start = direct_map_virtual_start(physical_start_bytes)?;
        kalloc::global_remove_memory(virtual_start, size_bytes as usize).map_err(allocator_error)
    }
}

fn direct_map_virtual_start(physical_start_bytes: u64) -> KResult<usize> {
    validate_memory_range(physical_start_bytes, 1)?;
    Ok(kaddr_layout::p2v(physical_start_bytes as usize))
}

/// Validates one hotplug block.
///
/// A memory device is exactly one [`MEMORY_BLOCK_SIZE_BYTES`] block aligned to
/// that size; `phys_index` and the sysfs bitmap are defined in those units.
fn validate_memory_block(
    id: SystemDeviceId,
    physical_start_bytes: u64,
    size_bytes: u64,
) -> KResult<()> {
    validate_memory_range(physical_start_bytes, size_bytes)?;
    if size_bytes != MEMORY_BLOCK_SIZE_BYTES
        || !physical_start_bytes.is_multiple_of(MEMORY_BLOCK_SIZE_BYTES)
        || physical_start_bytes / MEMORY_BLOCK_SIZE_BYTES != u64::from(id.index())
    {
        return Err(KErrorKind::InvalidInput.into());
    }
    Ok(())
}

fn validate_memory_range(physical_start_bytes: u64, size_bytes: u64) -> KResult<()> {
    if size_bytes == 0 || !physical_start_bytes.is_multiple_of(PAGE_SIZE_4K as u64) {
        return Err(KErrorKind::InvalidInput.into());
    }
    if !size_bytes.is_multiple_of(PAGE_SIZE_4K as u64) {
        return Err(KErrorKind::InvalidInput.into());
    }
    let end = physical_start_bytes
        .checked_add(size_bytes)
        .ok_or(KErrorKind::InvalidInput)?;
    if end > LINEAR_MAP_VSIZE as u64 {
        return Err(KErrorKind::InvalidInput.into());
    }
    Ok(())
}

/// Rejects a second online memory device whose physical range overlaps `state`.
fn validate_block_does_not_overlap(id: SystemDeviceId, start: u64, size: u64) -> KResult<()> {
    let end = start.checked_add(size).ok_or(KErrorKind::InvalidInput)?;
    for other in present_system_devices(SystemClass::Memory) {
        if other == id {
            continue;
        }
        let Some(other_state) = system_device_state(other) else {
            continue;
        };
        let other_end = other_state
            .physical_start_bytes
            .checked_add(other_state.size_bytes)
            .ok_or(KErrorKind::InvalidInput)?;
        if start < other_end && other_state.physical_start_bytes < end {
            return Err(KErrorKind::ResourceBusy.into());
        }
    }
    Ok(())
}

/// Requires the whole range to be described by `MemFlags::FREE` memory.
///
/// The page allocator is still the authority for whether individual pages are
/// free; this check only rejects ranges that were never usable RAM (MMIO,
/// reserved firmware memory, or a hole).
fn validate_range_is_free(start: u64, size: u64) -> KResult<()> {
    let end = start.checked_add(size).ok_or(KErrorKind::InvalidInput)?;
    let mut free_ranges: Vec<(u64, u64)> = khal::mem::memory_regions()
        .filter(|region| region.flags.contains(MemFlags::FREE))
        .filter_map(|region| {
            let region_start = region.paddr.as_usize() as u64;
            let region_end = region_start.checked_add(region.size as u64)?;
            if region_end <= start || region_start >= end {
                return None;
            }
            Some((region_start.max(start), region_end.min(end)))
        })
        .collect();
    free_ranges.sort_unstable();

    let mut covered_until = start;
    for (range_start, range_end) in free_ranges {
        if range_end <= covered_until {
            continue;
        }
        if range_start > covered_until {
            break;
        }
        covered_until = range_end;
        if covered_until >= end {
            return Ok(());
        }
    }
    Err(KErrorKind::InvalidInput.into())
}

/// Installs one device for every 128 MiB block that intersects usable RAM.
///
/// A block at the start of RAM may include holes or reserved pages. It is still
/// present and starts online because part of it is usable; the allocator probe
/// prevents it from ever going offline while those pages are in use.
pub(crate) fn install_boot_memory_blocks() {
    let mut block_starts = BTreeSet::new();
    for region in khal::mem::memory_regions() {
        if !region.flags.contains(MemFlags::FREE) || region.size == 0 {
            continue;
        }
        let region_start = region.paddr.as_usize() as u64;
        let Some(region_end) = region_start.checked_add(region.size as u64) else {
            continue;
        };
        let mut block_start = region_start / MEMORY_BLOCK_SIZE_BYTES * MEMORY_BLOCK_SIZE_BYTES;
        while block_start < region_end {
            block_starts.insert(block_start);
            block_start = match block_start.checked_add(MEMORY_BLOCK_SIZE_BYTES) {
                Some(next) => next,
                None => break,
            };
        }
    }

    for block_start in block_starts {
        let index = (block_start / MEMORY_BLOCK_SIZE_BYTES) as u32;
        let id = SystemDeviceId::new(SystemClass::Memory, index);
        if system_device_state(id).is_some() {
            continue;
        }
        let _ = install_system_device(SystemDeviceDescriptor::new(
            id,
            block_start,
            MEMORY_BLOCK_SIZE_BYTES,
            true,
            Arc::new(MemoryControl::new(id)),
        ));
    }
}
