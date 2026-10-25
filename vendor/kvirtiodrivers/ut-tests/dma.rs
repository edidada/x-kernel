use super::{BufferDirection, Dma, DmaBackend, Hal, PhysAddr, fake::FakeHal};
use crate::{Error, Result};
use alloc::rc::Rc;
use core::{cell::Cell, ptr::NonNull};

#[derive(Default)]
struct Accounting {
    allocations: Cell<usize>,
    releases: Cell<usize>,
    fail_allocation: Cell<bool>,
}

#[derive(Clone)]
struct Instance {
    address: PhysAddr,
    accounting: Rc<Accounting>,
}

// SAFETY: FakeHal supplies aligned zeroed allocations and matching releases.
// Each instance retains its own accounting across clones. Tests use only one
// live allocation per instance and never give these synthetic addresses to hardware.
unsafe impl DmaBackend for Instance {
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access_platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)> {
        if self.accounting.fail_allocation.get() {
            return Err(Error::DmaError);
        }
        let (_, pointer) = FakeHal::dma_alloc(pages, direction, access_platform);
        self.accounting
            .allocations
            .set(self.accounting.allocations.get() + 1);
        Ok((self.address, pointer))
    }

    unsafe fn deallocate(
        &self,
        address: PhysAddr,
        pointer: NonNull<u8>,
        pages: usize,
        _direction: BufferDirection,
        access_platform: bool,
    ) {
        assert_eq!(address, self.address);
        self.accounting
            .releases
            .set(self.accounting.releases.get() + 1);
        // SAFETY: The caller returns the FakeHal allocation with its original
        // pointer, page count and access policy; FakeHal uses the pointer to free it.
        unsafe {
            FakeHal::dma_dealloc(
                pointer.as_ptr() as PhysAddr,
                pointer,
                pages,
                access_platform,
            );
        }
    }

    unsafe fn share(
        &self,
        _buffer: NonNull<[u8]>,
        _direction: BufferDirection,
        _access_platform: bool,
    ) -> Result<PhysAddr> {
        Err(Error::DmaError)
    }

    unsafe fn unshare(
        &self,
        _address: PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: BufferDirection,
        _access_platform: bool,
    ) {
        panic!("this fixture never creates a shared mapping");
    }
}

#[test]
fn allocations_keep_their_instance_until_release() {
    let first = Instance {
        address: 0,
        accounting: Rc::default(),
    };
    let second = Instance {
        address: 0x4000,
        accounting: Rc::default(),
    };
    let first_memory =
        Dma::<FakeHal, _>::new_with_dma(1, BufferDirection::Both, false, first.clone()).unwrap();
    let second_memory =
        Dma::<FakeHal, _>::new_with_dma(2, BufferDirection::DriverToDevice, true, second.clone())
            .unwrap();
    assert_eq!(first_memory.paddr(), 0);
    assert_eq!(second_memory.paddr(), 0x4000);
    assert_ne!(first_memory.vaddr(0), second_memory.vaddr(0));
    drop(first_memory);
    assert_eq!(first.accounting.releases.get(), 1);
    assert_eq!(second.accounting.releases.get(), 0);
    drop(second_memory);
    assert_eq!(second.accounting.releases.get(), 1);
    assert_eq!(first.accounting.allocations.get(), 1);
    assert_eq!(second.accounting.allocations.get(), 1);
}

#[test]
fn allocation_failure_never_calls_release() {
    let instance = Instance {
        address: 0x8000,
        accounting: Rc::default(),
    };
    instance.accounting.fail_allocation.set(true);
    let result = Dma::<FakeHal, _>::new_with_dma(1, BufferDirection::Both, false, instance.clone());
    assert!(matches!(result, Err(Error::DmaError)));
    assert_eq!(instance.accounting.allocations.get(), 0);
    assert_eq!(instance.accounting.releases.get(), 0);
}

#[test]
fn invalid_size_is_rejected_before_backend_allocation() {
    let instance = Instance {
        address: 0x8000,
        accounting: Rc::default(),
    };
    for pages in [0, usize::MAX] {
        assert!(matches!(
            Dma::<FakeHal, _>::new_with_dma(pages, BufferDirection::Both, false, instance.clone()),
            Err(Error::InvalidParam)
        ));
    }
    assert_eq!(instance.accounting.allocations.get(), 0);
}
