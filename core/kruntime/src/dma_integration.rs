// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[kiface::provide]
impl kdma::DmaPageTableIf {
    fn poll_pending_tlb() {
        #[cfg(feature = "ipi")]
        {
            let _guard = kspin::NoPreemptIrqSave::new();
            kipi::tlb::handle_shootdown();
        }
    }

    fn query_contiguous(
        vaddr: memaddr::VirtAddr,
        size: usize,
    ) -> Option<(memaddr::PhysAddr, khal::paging::MappingFlags)> {
        if size == 0 {
            return None;
        }
        #[cfg(target_arch = "aarch64")]
        if kbuild_config::KFEAT_KVM_GUEST_MEM_SHARE {
            // Private guest RAM requires page sharing and must stay on the coherent backend.
            return None;
        }
        let end = vaddr.as_usize().checked_add(size)?;
        let _guard = kspin::NoPreemptIrqSave::new();
        let layout = memspace::kernel_layout();
        loop {
            if let Some(locked) = layout.try_lock() {
                return contiguous_ram_range(vaddr, end, |address| {
                    locked
                        .page_table()
                        .query(address)
                        .ok()
                        .map(|(physical, flags, size)| (physical, flags, size as usize))
                });
            }
            kdma::DmaPageTableIf::poll_pending_tlb();
            core::hint::spin_loop();
        }
    }

    fn protect(
        vaddr: memaddr::VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> kerrno::KResult {
        let _guard = kspin::NoPreemptIrqSave::new();
        let layout = memspace::kernel_layout();
        loop {
            if let Some(mut locked) = layout.try_lock() {
                return locked.protect(vaddr, size, flags);
            }
            // Another CPU can hold this lock until our IRQ-off CPU acknowledges its shootdown.
            kdma::DmaPageTableIf::poll_pending_tlb();
            core::hint::spin_loop();
        }
    }
}

/// Validate every covering leaf while the caller keeps its page table locked.
fn contiguous_ram_range(
    start: memaddr::VirtAddr,
    end: usize,
    mut query: impl FnMut(
        memaddr::VirtAddr,
    ) -> Option<(memaddr::PhysAddr, khal::paging::MappingFlags, usize)>,
) -> Option<(memaddr::PhysAddr, khal::paging::MappingFlags)> {
    use khal::paging::MappingFlags;
    let (first, flags, _) = query(start)?;
    if !flags.contains(MappingFlags::READ | MappingFlags::WRITE)
        || flags.intersects(MappingFlags::DEVICE | MappingFlags::USER)
    {
        return None;
    }
    let mut cursor = start.as_usize();
    while cursor < end {
        let (physical, current_flags, page_bytes) = query(cursor.into())?;
        if current_flags != flags
            || physical.as_usize() != first.as_usize().checked_add(cursor - start.as_usize())?
        {
            return None;
        }
        cursor = (cursor & !(page_bytes - 1))
            .checked_add(page_bytes)?
            .min(end);
    }
    Some((first, flags))
}

#[kiface::provide]
impl kdma::DmaCacheIf {
    fn policy() -> kdma::DmaCachePolicy {
        #[cfg(target_arch = "x86_64")]
        {
            kdma::DmaCachePolicy::Coherent
        }
        #[cfg(target_arch = "aarch64")]
        {
            kdma::DmaCachePolicy::NonCoherent {
                // Whole-page isolation avoids sharing higher-level cache/writeback
                // granules with neighboring suballocations in the unified domain.
                alignment: memaddr::PAGE_SIZE_4K.max(karch::data_cache_line_size()),
            }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            kdma::DmaCachePolicy::Unsupported
        }
    }

    fn maintain(
        range: &kdma::DmaCacheRange,
        operation: kdma::DmaCacheOperation,
    ) -> alloc_engine::AllocResult<()> {
        let start = range.start();
        let len = range.len();
        #[cfg(target_arch = "aarch64")]
        {
            let line = karch::data_cache_line_size();
            if len == 0
                || !start.as_usize().is_multiple_of(line)
                || !len.is_multiple_of(line)
                || start.as_usize().checked_add(len).is_none()
            {
                return Err(alloc_engine::AllocError::InvalidInput);
            }
            match operation {
                kdma::DmaCacheOperation::Clean => karch::clean_dcache_range_to_poc(start, len),
                kdma::DmaCacheOperation::Invalidate => {
                    // SAFETY: kdma pins exclusive live RAM with both owners stopped;
                    // checks above establish whole nonoverflowing cache-line endpoints.
                    unsafe { karch::invalidate_dcache_range_to_poc(start, len) };
                }
                kdma::DmaCacheOperation::CleanInvalidate => {
                    // SAFETY: kdma pins exclusive live RAM with both owners stopped;
                    // checks above establish whole nonoverflowing cache-line endpoints.
                    unsafe { karch::clean_invalidate_dcache_range_to_poc(start, len) };
                }
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        {
            let line =
                karch::data_cache_line_size().ok_or(alloc_engine::AllocError::InvalidInput)?;
            if len == 0
                || !start.as_usize().is_multiple_of(line)
                || !len.is_multiple_of(line)
                || start.as_usize().checked_add(len).is_none()
            {
                return Err(alloc_engine::AllocError::InvalidInput);
            }
            // CPU/device DMA is coherent; explicit maintenance is used for memory-type
            // transitions, where preserving dirty data before invalidation is required.
            let _ = operation;
            // SAFETY: kdma pins exclusive live RAM, checks above validate whole cache
            // lines, and CPUID established CLFLUSH availability before this call.
            unsafe { karch::clean_invalidate_dcache_range_to_poc(start, len) };
            Ok(())
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            let _ = (start, len, operation);
            Err(alloc_engine::AllocError::InvalidInput)
        }
    }
}

#[cfg(unittest)]
mod tests {
    use khal::paging::MappingFlags;
    use memaddr::{PhysAddr, VirtAddr};
    use unittest::{assert, def_test};

    use super::contiguous_ram_range;

    #[def_test]
    fn direct_range_rejects_discontinuous_mixed_and_missing_pages() {
        let start = VirtAddr::from_usize(0x1ff0);
        let end = 0x2010;
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        let contiguous = |address: VirtAddr| {
            Some((
                PhysAddr::from_usize(address.as_usize() + 0x10000),
                flags,
                4096,
            ))
        };
        assert!(contiguous_ram_range(start, end, contiguous).is_some());
        assert!(
            contiguous_ram_range(start, end, |address| {
                let (physical, flags, size) = contiguous(address)?;
                let gap = usize::from(address.as_usize() >= 0x2000) * 4096;
                Some((PhysAddr::from_usize(physical.as_usize() + gap), flags, size))
            })
            .is_none()
        );
        assert!(
            contiguous_ram_range(start, end, |address| {
                let (physical, flags, size) = contiguous(address)?;
                Some((
                    physical,
                    if address.as_usize() >= 0x2000 {
                        flags | MappingFlags::UNCACHED
                    } else {
                        flags
                    },
                    size,
                ))
            })
            .is_none()
        );
        assert!(
            contiguous_ram_range(start, end, |address| {
                (address.as_usize() < 0x2000)
                    .then(|| contiguous(address))
                    .flatten()
            })
            .is_none()
        );
        assert!(
            contiguous_ram_range(start, end, |address| {
                let (physical, flags, size) = contiguous(address)?;
                Some((physical, flags | MappingFlags::DEVICE, size))
            })
            .is_none()
        );
    }
}
