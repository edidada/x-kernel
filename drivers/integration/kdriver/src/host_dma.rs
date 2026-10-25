// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Coherent DMA adaptation for model device resource services.
//!
//! Enumeration owns the DMA view. This module owns native allocation pairing,
//! translating CPU physical addresses rather than reinterpreting native bus IDs.

use alloc::sync::Arc;
use core::{alloc::Layout, ptr::NonNull};

use driver_model::{CoherentMem, DeviceContext, DmaInfo, RuntimeError};
use khal::mem::v2p;
use memaddr::{PAGE_SIZE_4K, VirtAddr};

/// Allocate through the current callback's authoritative device DMA view.
///
/// Host `DeviceResourceServices` implementations delegate here. No DMA facts are
/// cached outside the descriptor. The whole page-rounded allocation must fit
/// its window and mask, and the translated address must satisfy `align`.
/// Memory is zeroed before publication, including page padding. The returned
/// guard frees using the original native bus token, not the translated address.
/// Callers must stop device access before dropping the guard.
///
/// # Errors
/// Missing configuration or non-coherent devices return `Unsupported`; this
/// adapter does not yet implement non-coherent cache maintenance. Invalid size,
/// alignment or mask returns `Invalid`. Allocation failures or an allocated
/// range outside the device's constraints return `NoMemory`, without retrying
/// the unconstrained allocator. Rejected allocations are released immediately.
/// Metadata allocation follows the kernel's infallible `Arc` allocation policy.
pub fn alloc_coherent(
    device: DeviceContext<'_>,
    len: usize,
    align: usize,
) -> Result<CoherentMem, RuntimeError> {
    allocate_for_view(device.device().descriptor().dma.as_ref(), len, align)
}

fn allocate_for_view(
    view: Option<&DmaInfo>,
    len: usize,
    align: usize,
) -> Result<CoherentMem, RuntimeError> {
    let view = view.ok_or(RuntimeError::Unsupported)?;
    if !view.coherent {
        return Err(RuntimeError::Unsupported);
    }
    if len == 0 || (view.mask != u64::MAX && view.mask & (view.mask + 1) != 0) {
        return Err(RuntimeError::Invalid);
    }
    let layout = Layout::from_size_align(len, align).map_err(|_| RuntimeError::Invalid)?;
    // SAFETY: the validated nonzero layout is owned by NativeAllocation until
    // transfer to the sole CoherentMem release hook; no device sees it yet.
    let native =
        unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| RuntimeError::NoMemory)?;
    publish(
        view,
        NativeAllocation {
            native,
            layout,
            release: Some(release_native),
        },
    )
}

fn publish<F>(view: &DmaInfo, allocation: NativeAllocation<F>) -> Result<CoherentMem, RuntimeError>
where
    F: Fn(kdma::DmaInfo, Layout) + Send + Sync + 'static,
{
    let native = allocation.native;
    let layout = allocation.layout;
    let span = layout.size().div_ceil(PAGE_SIZE_4K) * PAGE_SIZE_4K;
    let cpu = native.cpu_addr.as_ptr() as usize;
    let physical = v2p(VirtAddr::from_usize(cpu)).as_usize() as u64;
    let bus = translate(view, physical, span, layout.align())?;
    // SAFETY: kdma owns at least span page-rounded writable bytes at this
    // pointer. They are exclusively held here, before any device publication.
    unsafe { native.cpu_addr.as_ptr().write_bytes(0, span) };
    allocation.into_guard(bus)
}

fn translate(
    view: &DmaInfo,
    physical: u64,
    span: usize,
    align: usize,
) -> Result<u64, RuntimeError> {
    let bus = view
        .translate_range(physical, span as u64)
        .ok_or(RuntimeError::NoMemory)?;
    if bus & (align as u64 - 1) != 0 {
        return Err(RuntimeError::NoMemory);
    }
    Ok(bus)
}

struct NativeAllocation<F: Fn(kdma::DmaInfo, Layout)> {
    native: kdma::DmaInfo,
    layout: Layout,
    release: Option<F>,
}

impl<F: Fn(kdma::DmaInfo, Layout) + Send + Sync + 'static> NativeAllocation<F> {
    fn into_guard(mut self, bus: u64) -> Result<CoherentMem, RuntimeError> {
        let cpu = self.native.cpu_addr.addr();
        let native_bus = self.native.bus_addr;
        let layout = self.layout;
        let release_native = self.release.take().ok_or(RuntimeError::BadState)?;
        let release = Arc::new(move || {
            let native = kdma::DmaInfo {
                // SAFETY: cpu is the original nonzero allocation address. The
                // private hook is transferred to one non-Clone CoherentMem,
                // whose Drop invokes it once after device access has stopped.
                cpu_addr: unsafe { NonNull::new_unchecked(cpu.get() as *mut u8) },
                bus_addr: native_bus,
            };
            release_native(native, layout);
        });
        let guard = CoherentMem::new(cpu.get(), bus, layout.size(), Some(release));
        Ok(guard)
    }
}

impl<F: Fn(kdma::DmaInfo, Layout)> Drop for NativeAllocation<F> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release(self.native, self.layout);
        }
    }
}

fn release_native(native: kdma::DmaInfo, layout: Layout) {
    // SAFETY: only the unpublished owner or its transferred private guard hook
    // calls this function, once, with the original native allocation and layout.
    // Published users must stop device access before releasing their guard.
    unsafe { kdma::deallocate_dma_memory(native, layout) };
}

unittest::path_mod_test!(tests, "../ut-tests/host_dma.rs");
