// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`device_res::DmaOp`] provider contract.

use core::{alloc::Layout, ptr::NonNull};

use device_res::{DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec, ResError, ResResult};

use crate::XKernelResourceProvider;

static PLATFORM_DMA: XKernelDmaProvider = XKernelDmaProvider {
    context: kdma::DmaDeviceContext::platform_default(),
};

/// DMA provider binding one device's address limits to the shared platform backend.
pub struct XKernelDmaProvider {
    context: kdma::DmaDeviceContext,
}

impl XKernelDmaProvider {
    /// Build a device DMA provider from its streaming and coherent address masks.
    ///
    /// # Errors
    /// Returns `InvalidResource` when the supplied constraints are invalid.
    pub fn new(streaming_mask: u64, coherent_mask: u64) -> ResResult<Self> {
        Ok(Self {
            context: kdma::DmaDeviceContext::new(streaming_mask, coherent_mask)
                .map_err(map_dma_error)?,
        })
    }

    /// Bind the existing unrestricted platform address policy to a device.
    pub fn platform_default() -> Self {
        Self {
            context: kdma::DmaDeviceContext::platform_default(),
        }
    }

    /// Synchronize the original mapping through this provider's retained context.
    /// # Safety
    /// The mapping is active and accesses by the previous owner have stopped.
    unsafe fn sync(&self, mapping: DmaMapping, target: kdma::DmaSyncTarget) {
        let Some(pointer) = NonNull::new(mapping.cpu_addr as *mut u8) else {
            return;
        };
        let buffer = NonNull::slice_from_raw_parts(pointer, mapping.len);
        // SAFETY: the caller retains the live original buffer and completed the ownership handoff.
        let result = unsafe {
            kdma::sync_dma_buffer(
                &self.context,
                kdma::DmaBusAddress::new(mapping.bus_addr),
                buffer,
                map_dma_direction(mapping.direction),
                target,
            )
        };
        assert!(
            result.is_ok(),
            "DMA synchronization must match an active mapping"
        );
    }
}

fn map_dma_error(error: alloc_engine::AllocError) -> ResError {
    match error {
        alloc_engine::AllocError::InvalidInput => ResError::InvalidResource,
        _ => ResError::NoMemory,
    }
}

// SAFETY: kdma owns backing pages and validates complete ranges in this provider's context.
unsafe impl DmaOp for XKernelDmaProvider {
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        let layout =
            Layout::from_size_align(spec.len, spec.align).map_err(|_| ResError::InvalidResource)?;
        // SAFETY: `layout` is a valid non-zero layout and the returned buffer is
        // owned exclusively by the `DmaCoherent` handle that wraps this
        // allocation until it is freed via `free_coherent`.
        let info =
            unsafe { kdma::allocate_dma_memory(&self.context, layout) }.map_err(map_dma_error)?;
        Ok(DmaAllocation {
            cpu_addr: info.cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            spec,
        })
    }

    unsafe fn free_coherent(&self, alloc: DmaAllocation) {
        let Ok(layout) = Layout::from_size_align(alloc.spec.len, alloc.spec.align) else {
            return;
        };
        let info = kdma::DmaInfo {
            cpu_addr: {
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: The resource handle returns \
                              the original non-null CPU address supplied by this DMA provider"
                )]
                NonNull::new(alloc.cpu_addr as *mut u8)
                    .expect("coherent DMA allocation stored a null CPU address")
            },
            bus_addr: kdma::DmaBusAddress::new(alloc.bus_addr),
        };
        // SAFETY: `info` and `layout` describe a coherent buffer previously
        // returned by `alloc_coherent` for the same spec, and it is freed
        // exactly once when its owning handle is dropped.
        unsafe { kdma::deallocate_dma_memory(&self.context, info, layout) };
    }

    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let dir = map_dma_direction(direction);
        let len = buffer.len();
        let cpu_addr = buffer.cast::<u8>();
        // SAFETY: map_streaming requires initialized bytes and exclusive ownership
        // of this live buffer until the matching unmap on this provider.
        let info =
            unsafe { kdma::map_dma_buffer(&self.context, buffer, dir) }.map_err(map_dma_error)?;
        Ok(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            len,
            direction,
        })
    }

    unsafe fn unmap_streaming(&self, mapping: DmaMapping) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The resource handle returns the \
                      original non-null CPU address supplied by this DMA provider"
        )]
        let cpu_addr = NonNull::new(mapping.cpu_addr as *mut u8)
            .expect("streaming DMA mapping stored a null CPU address");
        let buffer = NonNull::slice_from_raw_parts(cpu_addr, mapping.len);
        // SAFETY: `mapping` describes a streaming mapping previously established
        // by `map_streaming`; `cpu_addr` + `len` reconstruct the original buffer.
        unsafe {
            kdma::unmap_dma_buffer(
                &self.context,
                kdma::DmaBusAddress::new(mapping.bus_addr),
                buffer,
                map_dma_direction(mapping.direction),
            );
        }
    }

    unsafe fn sync_for_cpu(&self, mapping: DmaMapping) {
        // SAFETY: caller supplies an active mapping after stopping device access.
        unsafe { self.sync(mapping, kdma::DmaSyncTarget::Cpu) };
    }

    unsafe fn sync_for_device(&self, mapping: DmaMapping) {
        // SAFETY: caller supplies an active mapping after stopping CPU access.
        unsafe { self.sync(mapping, kdma::DmaSyncTarget::Device) };
    }
}

// SAFETY: compatibility operations delegate to the single retained platform context.
unsafe impl DmaOp for XKernelResourceProvider {
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        PLATFORM_DMA.alloc_coherent(spec)
    }

    unsafe fn free_coherent(&self, allocation: DmaAllocation) {
        // SAFETY: the caller supplies the original allocation from this compatibility provider.
        unsafe { PLATFORM_DMA.free_coherent(allocation) };
    }

    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        // SAFETY: the caller retains exclusive ownership of this buffer until unmap.
        unsafe { PLATFORM_DMA.map_streaming(buffer, direction) }
    }

    unsafe fn unmap_streaming(&self, mapping: DmaMapping) {
        // SAFETY: mapping belongs to this compatibility provider and hardware has stopped.
        unsafe { PLATFORM_DMA.unmap_streaming(mapping) };
    }

    unsafe fn sync_for_cpu(&self, mapping: DmaMapping) {
        // SAFETY: hardware has stopped and mapping belongs to this provider.
        unsafe { PLATFORM_DMA.sync_for_cpu(mapping) };
    }

    unsafe fn sync_for_device(&self, mapping: DmaMapping) {
        // SAFETY: CPU accesses have stopped and mapping belongs to this provider.
        unsafe { PLATFORM_DMA.sync_for_device(mapping) };
    }
}

/// Translate a device-res DMA direction into the matching `kdma` direction.
fn map_dma_direction(d: DmaDirection) -> kdma::DmaDirection {
    match d {
        DmaDirection::DriverToDevice => kdma::DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver => kdma::DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional => kdma::DmaDirection::Bidirectional,
    }
}
