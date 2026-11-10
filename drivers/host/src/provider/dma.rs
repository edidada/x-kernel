// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_base::devres::host_api::DmaOp`] contract.

use core::{alloc::Layout, ptr::NonNull};

use driver_base::devres::host_api::{
    DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec, ResError, ResResult,
};

use super::XKernelResourceProvider;

impl DmaOp for XKernelResourceProvider {
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        let layout =
            Layout::from_size_align(spec.len, spec.align).map_err(|_| ResError::InvalidResource)?;
        // SAFETY: `layout` is a valid non-zero layout and the returned buffer is
        // owned exclusively by the `DmaCoherent` handle that wraps this
        // allocation until it is freed via `free_coherent`.
        let info = unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
        log_coherent_pte(
            info.cpu_addr.as_ptr() as usize,
            info.bus_addr.as_u64(),
            spec.len,
        );
        Ok(DmaAllocation {
            cpu_addr: info.cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            spec,
        })
    }

    fn free_coherent(&self, alloc: DmaAllocation) {
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
        unsafe { kdma::deallocate_dma_memory(info, layout) };
    }

    fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let dir = map_dma_direction(direction);
        // SAFETY: the caller guarantees `buffer` is a valid, live slice for the
        // duration of the mapping.
        let slice: &[u8] = unsafe { buffer.as_ref() };
        let len = slice.len();
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        // SAFETY: same contract - `buffer` is valid for the mapping duration.
        let info = unsafe { kdma::map_dma_buffer(buffer, dir) }.map_err(|_| ResError::NoMemory)?;
        Ok(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            len,
            direction,
        })
    }

    fn unmap_streaming(&self, mapping: DmaMapping) {
        if mapping.bus_addr == 0 {
            return;
        }

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
                kdma::DmaBusAddress::new(mapping.bus_addr),
                buffer,
                map_dma_direction(mapping.direction),
            );
        }
    }
}

/// Translate a driver-core provider DMA direction into the matching `kdma` direction.
fn map_dma_direction(d: DmaDirection) -> kdma::DmaDirection {
    match d {
        DmaDirection::DriverToDevice => kdma::DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver => kdma::DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional => kdma::DmaDirection::Bidirectional,
    }
}

/// Logs the page-table entry backing a coherent allocation, once per mapping.
///
/// A driver that hands a descriptor ring to a device has exactly one property
/// to rely on: the memory it writes is not sitting dirty in a CPU cache. The
/// DMA allocator asks for `UNCACHED|SHARED`, but that is a *request* -- this
/// prints what the hardware will actually walk, which is the difference between
/// "the device ignores our descriptors" and "the allocator's remap did not
/// take".
fn log_coherent_pte(cpu_addr: usize, bus_addr: u64, len: usize) {
    let va = memaddr::VirtAddr::from(cpu_addr);
    let aspace = memspace::kernel_layout().lock();
    match aspace.page_table().query_entry(va) {
        Ok(pte) => log::info!(
            "dma: coherent {len:#x}B cpu {cpu_addr:#x} bus {bus_addr:#x} pte paddr {:#x} {:?} {:?}",
            pte.paddr().as_usize(),
            pte.flags(),
            pte.page_size()
        ),
        Err(err) => log::warn!("dma: coherent {cpu_addr:#x} pte query failed: {err:?}"),
    }
}
