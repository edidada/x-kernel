// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable device constraints and a device-owned DMA mapping namespace.

use alloc_engine::{AllocError, AllocResult};
use kspin::SpinNoIrq;

use crate::{DmaBusAddress, dma::DmaAllocator};

/// Addressing and single-segment requirements declared by a driver.
#[derive(Clone, Copy, Debug)]
pub struct DmaConstraints {
    /// Mask for streaming mappings, consisting of contiguous low bits.
    pub streaming_mask: u64,
    /// Mask for persistent coherent allocations.
    pub coherent_mask: u64,
    /// Minimum device address alignment in bytes, a power of two.
    pub min_addr_align_bytes: usize,
    /// Maximum bytes in a single streaming segment.
    pub max_segment_len_bytes: usize,
    /// Low-bit mask of the boundary a streaming segment must stay within.
    pub segment_boundary_mask: u64,
}

impl DmaConstraints {
    /// Conservative address limits for the existing platform DMA backend.
    pub const DMA32: Self = Self {
        streaming_mask: u32::MAX as u64,
        coherent_mask: u32::MAX as u64,
        min_addr_align_bytes: 1,
        max_segment_len_bytes: usize::MAX,
        segment_boundary_mask: u64::MAX,
    };

    pub(crate) fn validate(self) -> AllocResult<()> {
        let is_mask = |mask: u64| mask != 0 && (mask == u64::MAX || (mask + 1).is_power_of_two());
        if !is_mask(self.streaming_mask)
            || !is_mask(self.coherent_mask)
            || !is_mask(self.segment_boundary_mask)
            || !self.min_addr_align_bytes.is_power_of_two()
            || self.max_segment_len_bytes == 0
        {
            return Err(AllocError::InvalidInput);
        }
        Ok(())
    }

    pub(crate) fn check_range(self, start: u64, len: usize, is_streaming: bool) -> AllocResult<()> {
        let end = start
            .checked_add(len.checked_sub(1).ok_or(AllocError::InvalidInput)? as u64)
            .ok_or(AllocError::InvalidInput)?;
        let mask = if is_streaming {
            self.streaming_mask
        } else {
            self.coherent_mask
        };
        if end > mask || !start.is_multiple_of(self.min_addr_align_bytes as u64) {
            return Err(AllocError::InvalidInput);
        }
        if is_streaming
            && (len > self.max_segment_len_bytes
                || (start & !self.segment_boundary_mask) != (end & !self.segment_boundary_mask))
        {
            return Err(AllocError::InvalidInput);
        }
        Ok(())
    }
}

/// A contiguous physical-to-device address window.
#[derive(Clone, Copy, Debug)]
pub struct DmaWindow {
    phys_start: u64,
    bus_start: u64,
    len_bytes: u64,
}

impl DmaWindow {
    /// Validate both complete address intervals before constructing a window.
    ///
    /// # Errors
    /// Returns `InvalidInput` for an empty window or an overflowing physical or bus interval.
    pub fn new(phys_start: u64, bus_start: u64, len_bytes: u64) -> AllocResult<Self> {
        let last = len_bytes.checked_sub(1).ok_or(AllocError::InvalidInput)?;
        phys_start
            .checked_add(last)
            .ok_or(AllocError::InvalidInput)?;
        bus_start
            .checked_add(last)
            .ok_or(AllocError::InvalidInput)?;
        Ok(Self {
            phys_start,
            bus_start,
            len_bytes,
        })
    }

    pub(crate) fn physical_range(self, mask: u64) -> AllocResult<(usize, usize)> {
        let available = mask
            .checked_sub(self.bus_start)
            .ok_or(AllocError::NoMemory)?;
        let bytes = self.len_bytes.min(available.saturating_add(1));
        let start = usize::try_from(self.phys_start).map_err(|_| AllocError::InvalidInput)?;
        let end = self
            .phys_start
            .checked_add(bytes)
            .ok_or(AllocError::InvalidInput)?;
        Ok((
            start,
            usize::try_from(end).map_err(|_| AllocError::InvalidInput)?,
        ))
    }

    /// Translate a complete contiguous physical interval through this window.
    ///
    /// # Errors
    /// Returns `InvalidInput` for an empty, overflowing or out-of-window interval.
    pub fn translate(self, phys_start: u64, len_bytes: usize) -> AllocResult<DmaBusAddress> {
        let offset = phys_start
            .checked_sub(self.phys_start)
            .ok_or(AllocError::InvalidInput)?;
        let end = offset
            .checked_add(len_bytes as u64)
            .ok_or(AllocError::InvalidInput)?;
        if len_bytes == 0 || end > self.len_bytes {
            return Err(AllocError::InvalidInput);
        }
        Ok(DmaBusAddress::new(self.bus_start + offset))
    }
}

/// Device-owned constraints, translation and bounce-mapping state.
///
/// Each instance owns a separate mapping namespace and lazily allocated bounce
/// pool. All buffers use uncached shared pages; hardware-coherent direct mapping
/// is deliberately deferred until buffer provenance and cache contracts exist.
/// Keep this context alive until all allocations and mappings have been released.
pub struct DmaDeviceContext {
    allocator: SpinNoIrq<DmaAllocator>,
}

impl DmaDeviceContext {
    pub(crate) fn lock(&self) -> kspin::SpinNoIrqGuard<'_, DmaAllocator> {
        loop {
            if let Some(guard) = self.allocator.try_lock() {
                return guard;
            }
            // The owner may be changing DMA page attributes and waiting for this CPU's TLB ack.
            crate::DmaPageTableIf::poll_pending_tlb();
            core::hint::spin_loop();
        }
    }

    /// Construct a context after validating the driver's immutable limits.
    ///
    /// # Errors
    /// Returns `InvalidInput` for invalid low-bit masks, a non-power-of-two
    /// alignment or an empty maximum segment length.
    pub fn new(constraints: DmaConstraints, window: DmaWindow) -> AllocResult<Self> {
        constraints.validate()?;
        Ok(Self {
            allocator: SpinNoIrq::new(DmaAllocator::new(constraints, window)),
        })
    }

    /// Compatibility context for callers using the platform's DMA32 window.
    ///
    /// New device integration should construct a distinct context with `new`.
    pub const fn platform_default() -> Self {
        Self {
            allocator: SpinNoIrq::new(DmaAllocator::new(
                DmaConstraints::DMA32,
                DmaWindow {
                    phys_start: 0,
                    bus_start: kbuild_config::PHYS_BUS_OFFSET as u64,
                    len_bytes: if (kbuild_config::PHYS_BUS_OFFSET as u64) > u64::MAX - 0xffff_ffff {
                        u64::MAX - kbuild_config::PHYS_BUS_OFFSET as u64 + 1
                    } else {
                        0x1_0000_0000
                    },
                },
            )),
        }
    }
}

impl Drop for DmaDeviceContext {
    fn drop(&mut self) {
        self.lock().release_bounce_pool();
    }
}
