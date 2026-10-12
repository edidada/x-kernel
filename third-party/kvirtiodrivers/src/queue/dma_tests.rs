//! Device routing and transactional failure regression tests.
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::*;
use crate::{
    DmaProvider,
    hal::fake::FakeHal,
    transport::{
        DeviceType,
        fake::{FakeTransport, QueueStatus, State},
    },
};

#[derive(Debug, Default)]
struct Provider {
    allocations: Mutex<BTreeSet<usize>>,
    mappings: Mutex<BTreeSet<u64>>,
    allocation_calls: AtomicUsize,
    mapping_calls: AtomicUsize,
    fail_allocation: usize,
    fail_mapping: usize,
}

// SAFETY: backing allocation and mapping operations use FakeHal; sets enforce exact provider pairing.
unsafe impl DmaProvider for Provider {
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        access: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)> {
        if self.allocation_calls.fetch_add(1, Ordering::Relaxed) + 1 == self.fail_allocation {
            return Err(Error::DmaError);
        }
        let pair = FakeHal::dma_alloc(pages, direction, access);
        assert!(
            self.allocations
                .lock()
                .unwrap()
                .insert(pair.1.as_ptr() as usize)
        );
        Ok(pair)
    }

    unsafe fn deallocate(&self, addr: PhysAddr, ptr: NonNull<u8>, pages: usize, access: bool) {
        assert!(
            self.allocations
                .lock()
                .unwrap()
                .remove(&(ptr.as_ptr() as usize))
        );
        // SAFETY: the set confirms this allocation belongs to the provider, and the queue has stopped.
        assert_eq!(unsafe { FakeHal::dma_dealloc(addr, ptr, pages, access) }, 0);
    }

    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        dir: BufferDirection,
        access: bool,
    ) -> Result<PhysAddr> {
        if self.mapping_calls.fetch_add(1, Ordering::Relaxed) + 1 == self.fail_mapping {
            return Err(Error::DmaError);
        }
        // SAFETY: the test keeps submitted buffers live until completion or failed publication.
        let addr = unsafe { FakeHal::share(buffer, dir, access) };
        assert!(self.mappings.lock().unwrap().insert(addr));
        Ok(addr)
    }

    unsafe fn unshare(
        &self,
        addr: PhysAddr,
        buffer: NonNull<[u8]>,
        dir: BufferDirection,
        access: bool,
    ) {
        assert!(self.mappings.lock().unwrap().remove(&addr));
        // SAFETY: this mapping is active on this provider and hardware has completed or never started.
        unsafe { FakeHal::unshare(addr, buffer, dir, access) };
    }
}

fn transport(provider: Arc<Provider>) -> FakeTransport<()> {
    let mut state = State::new(vec![QueueStatus::default()], ());
    state.dma = Some(provider);
    FakeTransport {
        device_type: DeviceType::Block,
        max_queue_size: 8,
        device_features: 0,
        state: Arc::new(Mutex::new(state)),
    }
}

#[test]
fn failed_streaming_and_indirect_maps_leave_queue_unpublished() {
    for (indirect, failure) in [(false, 2), (true, 2), (true, 3)] {
        let provider = Arc::new(Provider {
            fail_mapping: failure,
            ..Provider::default()
        });
        let mut transport = transport(provider.clone());
        let mut queue =
            VirtQueue::<FakeHal, 8>::new(&mut transport, 0, indirect, false, false).unwrap();
        let first = [1u8; 8];
        let second = [2u8; 8];
        // SAFETY: inputs live through this call, which fails before publishing descriptors.
        assert_eq!(
            unsafe { queue.add(&[&first, &second], &mut []) },
            Err(Error::DmaError)
        );
        assert_eq!(queue.num_used, 0);
        assert_eq!(queue.free_head, 0);
        assert_eq!(queue.avail_idx, 0);
        assert!(provider.mappings.lock().unwrap().is_empty());
        // The same queue remains usable after rollback.
        // SAFETY: both arrays stay live through the matching pop below.
        let token = unsafe { queue.add(&[&first, &second], &mut []) }.unwrap();
        assert_eq!(
            transport.state.lock().unwrap().read_from_queue::<8>(0),
            [first, second].concat()
        );
        // SAFETY: the fake device completed the original descriptor chain.
        unsafe { queue.pop_used(token, &[&first, &second], &mut []) }.unwrap();
        assert!(provider.mappings.lock().unwrap().is_empty());
        drop(queue);
        assert!(provider.allocations.lock().unwrap().is_empty());
    }
}

#[test]
fn second_queue_allocation_failure_releases_first_region() {
    let provider = Arc::new(Provider {
        fail_allocation: 2,
        ..Provider::default()
    });
    let mut transport = transport(provider.clone());
    assert!(VirtQueue::<FakeHal, 8>::new(&mut transport, 0, false, false, false).is_err());
    assert!(provider.allocations.lock().unwrap().is_empty());
    assert!(!transport.queue_used(0));
}

#[test]
fn two_devices_retain_separate_providers_through_completion() {
    let first = Arc::new(Provider::default());
    let second = Arc::new(Provider::default());
    let mut t1 = transport(first.clone());
    let mut t2 = transport(second.clone());
    let mut q1 = VirtQueue::<FakeHal, 8>::new(&mut t1, 0, true, false, false).unwrap();
    let mut q2 = VirtQueue::<FakeHal, 8>::new(&mut t2, 0, false, false, false).unwrap();
    let b1 = [1u8; 8];
    let b2 = [2u8; 8];
    // SAFETY: b1 remains live until q1's completion below.
    let id1 = unsafe { q1.add(&[&b1], &mut []) }.unwrap();
    // SAFETY: b2 remains live until q2's completion below.
    let id2 = unsafe { q2.add(&[&b2], &mut []) }.unwrap();
    assert_eq!(first.mappings.lock().unwrap().len(), 1);
    assert_eq!(second.mappings.lock().unwrap().len(), 1);
    assert_eq!(t2.state.lock().unwrap().read_from_queue::<8>(0), b2);
    // SAFETY: q2's device has completed its original buffer.
    unsafe { q2.pop_used(id2, &[&b2], &mut []) }.unwrap();
    assert_eq!(first.mappings.lock().unwrap().len(), 1);
    assert!(second.mappings.lock().unwrap().is_empty());
    assert_eq!(t1.state.lock().unwrap().read_from_queue::<8>(0), b1);
    drop(t1);
    // SAFETY: completion is recorded in q1's retained ring; transport destruction preserves its provider.
    unsafe { q1.pop_used(id1, &[&b1], &mut []) }.unwrap();
    drop(q1);
    drop(q2);
    assert!(first.allocations.lock().unwrap().is_empty());
    assert!(second.allocations.lock().unwrap().is_empty());
}
