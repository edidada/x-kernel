// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult, BaseAllocator, ByteAllocator, TlsfByteAllocator};
use memaddr::PAGE_SIZE_4K;

use crate::{DmaBusAddress, DmaConstraints, DmaDirection, DmaInfo};

pub(super) const DMA_BOUNCE_POOL_PAGES: usize = 1024;

pub(super) struct BouncePool {
    cpu_addr: usize,
    bus_addr: DmaBusAddress,
    size: usize,
    allocator: TlsfByteAllocator,
}

pub(super) struct BounceMapping {
    pub allocation_cpu_addr: usize,
    pub original_cpu_addr: usize,
    pub direction: DmaDirection,
    pub cpu_addr: usize,
    pub len: usize,
    pub layout: Layout,
}

impl BouncePool {
    pub(super) fn new(dma: DmaInfo, size: usize) -> Self {
        let mut allocator = TlsfByteAllocator::new();
        allocator.init_region(dma.cpu_addr.as_ptr() as usize, size);
        Self {
            cpu_addr: dma.cpu_addr.as_ptr() as usize,
            bus_addr: dma.bus_addr,
            size,
            allocator,
        }
    }

    pub(super) fn allocate(
        &mut self,
        len: usize,
        constraints: DmaConstraints,
    ) -> AllocResult<(DmaInfo, Layout, usize)> {
        let boundary_alignment = if constraints.segment_boundary_mask == u64::MAX {
            1
        } else {
            usize::try_from(constraints.segment_boundary_mask + 1)
                .map_err(|_| AllocError::InvalidInput)?
        };
        let align = constraints.min_addr_align_bytes.max(boundary_alignment);
        let size = len.checked_add(align - 1).ok_or(AllocError::InvalidInput)?;
        let layout = Layout::from_size_align(size, 1).map_err(|_| AllocError::InvalidInput)?;
        let allocation = self.allocator.allocate(layout)?;
        let allocation_cpu_addr = allocation.as_ptr() as usize;
        let offset = allocation_cpu_addr - self.cpu_addr;
        let candidate = self.bus_addr.as_u64() + offset as u64;
        let padding = candidate.wrapping_neg() & (align as u64 - 1);
        let Some(aligned) = candidate.checked_add(padding) else {
            self.deallocate(allocation_cpu_addr, layout);
            return Err(AllocError::InvalidInput);
        };
        let bus_addr = DmaBusAddress::new(aligned);
        if let Err(error) = constraints.check_range(bus_addr.as_u64(), len, true) {
            self.deallocate(allocation_cpu_addr, layout);
            return Err(error);
        }
        // SAFETY: padding is at most align - 1; the allocated extent contains padding + len bytes.
        let cpu_addr = unsafe { allocation.add(padding as usize) };
        Ok((DmaInfo { cpu_addr, bus_addr }, layout, allocation_cpu_addr))
    }

    pub(super) fn backing(&self) -> DmaInfo {
        // SAFETY: the pool retains its original non-null page allocation until teardown.
        let cpu_addr = unsafe { NonNull::new_unchecked(self.cpu_addr as *mut u8) };
        DmaInfo {
            cpu_addr,
            bus_addr: self.bus_addr,
        }
    }

    pub(super) fn deallocate(&mut self, cpu_addr: usize, layout: Layout) {
        assert!(
            self.contains_cpu_addr(cpu_addr),
            "DMA bounce buffer {:#x} is outside the configured bounce pool",
            cpu_addr
        );
        // SAFETY: `cpu_addr` was validated to belong to this pool and `layout`
        // comes from the matching allocation path.
        unsafe {
            self.allocator
                .deallocate(NonNull::new_unchecked(cpu_addr as *mut u8), layout);
        }
    }

    fn contains_cpu_addr(&self, cpu_addr: usize) -> bool {
        (self.cpu_addr..self.cpu_addr + self.size).contains(&cpu_addr)
    }
}

pub(super) fn bounce_pool_size() -> usize {
    DMA_BOUNCE_POOL_PAGES * PAGE_SIZE_4K
}
