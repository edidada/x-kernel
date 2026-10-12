#[cfg(test)]
pub mod fake;

use core::{marker::PhantomData, ptr::NonNull};

use crate::{Error, PAGE_SIZE, Result};

/// A physical address as used for virtio.
pub type PhysAddr = u64;

/// Device-bound DMA operations supplied by the transport owner.
///
/// # Safety
/// Implementations must return live, exclusive, zeroed coherent allocations;
/// mappings must cover the complete buffer and remain valid until unshared.
/// All operations must use the same device domain and support cross-thread cleanup.
pub unsafe trait DmaProvider: core::fmt::Debug + Send + Sync {
    /// Allocate zeroed page-aligned DMA memory for this device.
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)>;
    /// Release an allocation after hardware access has stopped.
    /// # Safety
    /// Address, pointer and pages must match an unreleased allocation from this provider.
    unsafe fn deallocate(
        &self,
        addr: PhysAddr,
        ptr: NonNull<u8>,
        pages: usize,
        access_platform: bool,
    );
    /// Map a buffer, returning an error before exposing an invalid device address.
    /// # Safety
    /// Buffer must contain initialized bytes and stay live and exclusively
    /// DMA-owned until unshare or rollback.
    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<PhysAddr>;
    /// Return a mapping to the CPU after completion or before failed publication.
    /// # Safety
    /// Arguments must match an active mapping from this provider; hardware access has stopped.
    unsafe fn unshare(
        &self,
        addr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    );
}

/// Retains the provider selected by a transport, with a legacy static HAL fallback.
pub(crate) struct DmaBackend<H: Hal> {
    #[cfg(feature = "alloc")]
    provider: Option<alloc::sync::Arc<dyn DmaProvider>>,
    hal: PhantomData<H>,
}

impl<H: Hal> Clone for DmaBackend<H> {
    fn clone(&self) -> Self {
        Self {
            #[cfg(feature = "alloc")]
            provider: self.provider.clone(),
            hal: PhantomData,
        }
    }
}

impl<H: Hal> core::fmt::Debug for DmaBackend<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("DmaBackend")
    }
}

impl<H: Hal> DmaBackend<H> {
    pub(crate) fn legacy() -> Self {
        Self {
            #[cfg(feature = "alloc")]
            provider: None,
            hal: PhantomData,
        }
    }

    pub(crate) fn from_transport(transport: &impl crate::transport::Transport) -> Self {
        let _ = transport;
        Self {
            #[cfg(feature = "alloc")]
            provider: transport.dma_provider(),
            hal: PhantomData,
        }
    }

    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)> {
        #[cfg(feature = "alloc")]
        if let Some(provider) = &self.provider {
            return provider.allocate(pages, direction, access_platform);
        }
        let allocation = H::dma_alloc(pages, direction, access_platform);
        if allocation.0 == 0 {
            return Err(Error::DmaError);
        }
        Ok(allocation)
    }

    /// # Safety
    /// The arguments identify a live allocation created by this backend, with DMA stopped.
    unsafe fn deallocate(
        &self,
        addr: PhysAddr,
        ptr: NonNull<u8>,
        pages: usize,
        access_platform: bool,
    ) {
        #[cfg(feature = "alloc")]
        if let Some(provider) = &self.provider {
            // SAFETY: forwarded matching-allocation contract; provider is retained from allocation.
            unsafe { provider.deallocate(addr, ptr, pages, access_platform) };
            return;
        }
        // SAFETY: forwarded matching-allocation contract for the legacy backend.
        let err = unsafe { H::dma_dealloc(addr, ptr, pages, access_platform) };
        assert_eq!(err, 0, "failed to deallocate DMA");
    }

    /// # Safety
    /// Buffer remains valid and exclusively owned until this backend unshares it.
    pub(crate) unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<PhysAddr> {
        #[cfg(feature = "alloc")]
        if let Some(provider) = &self.provider {
            // SAFETY: the caller retains the original buffer through completion or rollback.
            return unsafe { provider.share(buffer, direction, access_platform) };
        }
        // SAFETY: the same lifetime and exclusivity contract applies to the legacy HAL.
        let addr = unsafe { H::share(buffer, direction, access_platform) };
        if addr == 0 {
            return Err(Error::DmaError);
        }
        Ok(addr)
    }

    /// # Safety
    /// Arguments match an active mapping from this backend and DMA has stopped.
    pub(crate) unsafe fn unshare(
        &self,
        addr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) {
        #[cfg(feature = "alloc")]
        if let Some(provider) = &self.provider {
            // SAFETY: original mapping arguments and retained provider are supplied by the queue.
            unsafe { provider.unshare(addr, buffer, direction, access_platform) };
            return;
        }
        // SAFETY: original mapping arguments identify a live legacy HAL mapping.
        unsafe { H::unshare(addr, buffer, direction, access_platform) };
    }
}

/// A region of contiguous physical memory used for DMA.
#[derive(Debug)]
pub struct Dma<H: Hal> {
    paddr: PhysAddr,
    vaddr: NonNull<u8>,
    pages: usize,
    access_platform: bool,
    backend: DmaBackend<H>,
}

// SAFETY: DMA memory can be accessed from any thread.
unsafe impl<H: Hal> Send for Dma<H> {}

// SAFETY: `&Dma` only allows pointers and physical addresses to be returned. Any actual access to
// the memory requires unsafe code, which is responsible for avoiding data races.
unsafe impl<H: Hal> Sync for Dma<H> {}

impl<H: Hal> Dma<H> {
    /// Allocates the given number of pages of physically contiguous memory to be used for DMA in
    /// the given direction.
    ///
    /// The pages will be zeroed.
    pub fn new(pages: usize, direction: BufferDirection, access_platform: bool) -> Result<Self> {
        Self::new_with_backend(pages, direction, access_platform, DmaBackend::legacy())
    }

    pub(crate) fn new_with_backend(
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
        backend: DmaBackend<H>,
    ) -> Result<Self> {
        let (paddr, vaddr) = backend.allocate(pages, direction, access_platform)?;
        Ok(Self {
            paddr,
            vaddr,
            pages,
            access_platform,
            backend,
        })
    }

    /// Returns the physical address of the start of the DMA region, as seen by devices.
    pub fn paddr(&self) -> PhysAddr {
        self.paddr
    }

    /// Returns a pointer to the given offset within the DMA region.
    pub fn vaddr(&self, offset: usize) -> NonNull<u8> {
        assert!(offset < self.pages * PAGE_SIZE);
        NonNull::new((self.vaddr.as_ptr() as usize + offset) as _).unwrap()
    }

    /// Returns a pointer to the entire DMA region as a slice.
    pub fn raw_slice(&self) -> NonNull<[u8]> {
        let raw_slice =
            core::ptr::slice_from_raw_parts_mut(self.vaddr(0).as_ptr(), self.pages * PAGE_SIZE);
        NonNull::new(raw_slice).unwrap()
    }
}

impl<H: Hal> Drop for Dma<H> {
    fn drop(&mut self) {
        // SAFETY: The memory was previously allocated by `dma_alloc` in `Dma::new`,
        // not yet deallocated, and we are passing the values from then.
        unsafe {
            self.backend
                .deallocate(self.paddr, self.vaddr, self.pages, self.access_platform)
        };
    }
}

/// The interface which a particular hardware implementation must implement.
///
/// # Safety
///
/// Implementations of this trait must follow the "implementation safety" requirements documented
/// for each method. Callers must follow the safety requirements documented for the unsafe methods.
pub unsafe trait Hal {
    /// Allocates and zeroes the given number of contiguous physical pages of DMA memory for VirtIO
    /// use.
    ///
    /// Returns both the physical address which the device can use to access the memory, and a
    /// pointer to the start of it which the driver can use to access it.
    ///
    /// # Implementation safety
    ///
    /// Implementations of this method must ensure that the `NonNull<u8>` returned is a
    /// [_valid_](https://doc.rust-lang.org/std/ptr/index.html#safety) pointer, aligned to
    /// [`PAGE_SIZE`], and won't alias any other allocations or references in the program until it
    /// is deallocated by `dma_dealloc`. The pages must be zeroed.
    fn dma_alloc(
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> (PhysAddr, NonNull<u8>);

    /// Deallocates the given contiguous physical DMA memory pages.
    ///
    /// # Safety
    ///
    /// The memory must have been allocated by `dma_alloc` on the same `Hal` implementation, and not
    /// yet deallocated. `pages` must be the same number passed to `dma_alloc` originally, and both
    /// `paddr` and `vaddr` must be the values returned by `dma_alloc`.
    unsafe fn dma_dealloc(
        paddr: PhysAddr,
        vaddr: NonNull<u8>,
        pages: usize,
        access_platform: bool,
    ) -> i32;

    /// Converts a physical address used for MMIO to a virtual address which the driver can access.
    ///
    /// This is only used for MMIO addresses within BARs read from the device, for the PCI
    /// transport. It may check that the address range up to the given size is within the region
    /// expected for MMIO.
    ///
    /// # Implementation safety
    ///
    /// Implementations of this method must ensure that the `NonNull<u8>` returned is a
    /// [_valid_](https://doc.rust-lang.org/std/ptr/index.html#safety) pointer, and won't alias any
    /// other allocations or references in the program.
    ///
    /// # Safety
    ///
    /// The `paddr` and `size` must describe a valid MMIO region. The implementation may validate it
    /// in some way (and panic if it is invalid) but is not guaranteed to.
    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8>;

    /// Shares the given memory range with the device, and returns the physical address that the
    /// device can use to access it.
    ///
    /// This may involve mapping the buffer into an IOMMU, giving the host permission to access the
    /// memory, or copying it to a special region where it can be accessed.
    ///
    /// # Safety
    ///
    /// The buffer must be a valid pointer to a non-empty memory range which will not be accessed by
    /// any other thread for the duration of this method call.
    unsafe fn share(
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    ) -> PhysAddr;

    /// Unshares the given memory range from the device and (if necessary) copies it back to the
    /// original buffer.
    ///
    /// # Safety
    ///
    /// The buffer must be a valid pointer to a non-empty memory range which will not be accessed by
    /// any other thread for the duration of this method call. The `paddr` must be the value
    /// previously returned by the corresponding `share` call.
    unsafe fn unshare(
        paddr: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        access_platform: bool,
    );
}

/// The direction in which a buffer is passed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BufferDirection {
    /// The buffer may be read or written by the driver, but only read by the device.
    DriverToDevice,
    /// The buffer may be read or written by the device, but only read by the driver.
    DeviceToDriver,
    /// The buffer may be read or written by both the device and the driver.
    Both,
}
