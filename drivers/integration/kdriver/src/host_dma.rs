// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-scoped DMA adaptation for model device resource services.
//!
//! Enumeration owns the DMA view. This module owns native allocation pairing,
//! translating CPU physical addresses rather than reinterpreting native bus IDs.

use alloc::sync::Arc;
use core::{alloc::Layout, ptr::NonNull};

use driver_model::{CoherentMem, DeviceContext, DeviceRef, DmaInfo, RuntimeError};
use driver_os_api::{DeviceDma, DmaDirection, ResError, ResResult, StreamingMapping};
use khal::mem::v2p;
use memaddr::{PAGE_SIZE_4K, VirtAddr};

/// Retain the original device for coherent allocation and streaming mappings.
///
/// Every operation reads the retained descriptor, not a cached DMA view or a
/// replacement domain service. Construction follows the Arc OOM policy. Guards
/// own native release tokens independently of the capability's lifetime.
///
/// Construct in normal probe/task context after native allocator and mapping
/// initialization. Acquisitions and guard release are non-sleeping and support
/// request-spinlock and ordinary IRQ contexts, but not NMI. kdma, page-table
/// and allocator spinlocks can be acquired, including lazy bounce-pool setup;
/// callers must not already hold those locks or locks ordered after them.
/// Spinlock contention is not latency-bounded. Capability/release-hook Arc
/// allocation and native mapping metadata follow the kernel's infallible
/// metadata-allocation policy, not an allocation-free or bounded-latency promise.
///
/// # Errors
/// Missing or non-coherent configuration returns `Unsupported`; an invalid
/// device mask returns `Invalid`. This does not activate hardware or revoke
/// access on removal: the driver must stop DMA before releasing its guards.
pub fn new_device_dma(device: DeviceContext<'_>) -> Result<Arc<dyn DeviceDma>, RuntimeError> {
    validate_view(device.device().descriptor().dma.as_ref())?;
    Ok(Arc::new(HostDeviceDma {
        device: device.device().clone(),
    }))
}

struct HostDeviceDma {
    device: DeviceRef,
}

// SAFETY: All operations consult the retained device's immutable DMA view.
// Coherent guards own original native tokens; streaming guards retain the exact
// buffer tuple and native bounce token until one unmap. kdma serializes native
// operations and provides global mappings. Callers retain exclusive buffer
// access and stop hardware before guard destruction as required by DeviceDma.
unsafe impl DeviceDma for HostDeviceDma {
    fn alloc_coherent(&self, len: usize, align: usize) -> ResResult<CoherentMem> {
        allocate_for_view(self.device.descriptor().dma.as_ref(), len, align).map_err(resource_error)
    }

    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<StreamingMapping> {
        // SAFETY: forwards the caller's live, exclusive buffer and stopped-device
        // release obligations to the native mapping owner.
        unsafe { map_streaming_for_view(self.device.descriptor().dma.as_ref(), buffer, direction) }
    }
}

fn validate_view(view: Option<&DmaInfo>) -> Result<&DmaInfo, RuntimeError> {
    let view = view.ok_or(RuntimeError::Unsupported)?;
    if !view.coherent {
        return Err(RuntimeError::Unsupported);
    }
    if view.mask != u64::MAX && view.mask & (view.mask + 1) != 0 {
        return Err(RuntimeError::Invalid);
    }
    Ok(view)
}

fn resource_error(error: RuntimeError) -> ResError {
    match error {
        RuntimeError::Unsupported => ResError::Unsupported,
        RuntimeError::Invalid => ResError::InvalidResource,
        RuntimeError::NoMemory => ResError::NoMemory,
        RuntimeError::BadState => ResError::BadState,
        _ => ResError::Io,
    }
}

/// Acquire a native mapping and publish it only after device-view validation.
///
/// # Safety
/// The caller must keep the entire buffer live and exclusive until the returned
/// guard is dropped, with all hardware access stopped before Drop. Acquisition
/// and cleanup must obey the contexts documented by [`new_device_dma`]. On error
/// no mapping remains and the caller may access the unchanged buffer again.
unsafe fn map_streaming_for_view(
    view: Option<&DmaInfo>,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) -> ResResult<StreamingMapping> {
    let view = validate_view(view).map_err(resource_error)?;
    let len = buffer.len();
    if len == 0 {
        return Err(ResError::InvalidResource);
    }
    let direction = match direction {
        DmaDirection::DriverToDevice => kdma::DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver => kdma::DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional => kdma::DmaDirection::Bidirectional,
    };
    // SAFETY: the caller retains this live exclusive buffer until the returned
    // guard unmaps it; no device has received the bounce address yet.
    let native =
        unsafe { kdma::map_dma_buffer(buffer, direction) }.map_err(|_| ResError::NoMemory)?;
    if direction == kdma::DmaDirection::DeviceToDriver {
        // SAFETY: native owns a live writable bounce interval of len bytes,
        // disjoint from the original buffer and not yet published to hardware.
        // Initialize unwritten RX bytes before a later full-length copy-back.
        unsafe { native.cpu_addr.as_ptr().write_bytes(0, len) };
    }
    // SAFETY: native is the unique unpublished mapping for this exact buffer
    // and direction; publication consumes it on both success and rejection.
    unsafe { publish_streaming(view, native, buffer, direction) }
}

/// Validate and transfer one unpublished native streaming mapping.
///
/// # Safety
/// `native`, `buffer` and `direction` must be the exact tuple from one live
/// kdma mapping, not yet exposed to hardware. The entire bounce interval must
/// contain initialized bytes. This consumes that mapping even
/// on error. On success the caller retains the exclusive buffer until guard
/// destruction, which must follow the end of all device access.
unsafe fn publish_streaming(
    view: &DmaInfo,
    native: kdma::DmaInfo,
    buffer: NonNull<[u8]>,
    direction: kdma::DmaDirection,
) -> ResResult<StreamingMapping> {
    let len = buffer.len();
    let physical = v2p(VirtAddr::from_usize(native.cpu_addr.as_ptr() as usize)).as_usize() as u64;
    let Some(bus) = view.translate_range(physical, len as u64) else {
        // SAFETY: this exact mapping is still unpublished. Discard consumes it
        // once without copying unused receive data into the original buffer.
        unsafe { kdma::discard_dma_buffer(native.bus_addr, buffer) };
        return Err(ResError::NoMemory);
    };
    let original = buffer.as_ptr() as *mut u8 as usize;
    let native_bus = native.bus_addr;
    let release = Arc::new(move || {
        // SAFETY: original is the caller's nonnull buffer address, retained live
        // for len bytes until this sole guard releases its matching mapping.
        let pointer = unsafe { NonNull::new_unchecked(original as *mut u8) };
        let buffer = NonNull::slice_from_raw_parts(pointer, len);
        // SAFETY: the private hook consumes the original native token, buffer,
        // length and direction once, after the caller has stopped device access.
        unsafe { kdma::unmap_dma_buffer(native_bus, buffer, direction) };
    });
    // SAFETY: bus covers the entire checked bounce interval. The private hook
    // owns its unique native mapping and performs directional copy-back once.
    Ok(unsafe { StreamingMapping::new(bus, len, release) })
}

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
    let view = validate_view(view)?;
    if len == 0 {
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
