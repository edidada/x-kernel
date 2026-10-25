use super::*;
use crate::{
    hal::fake::FakeHal,
    transport::{
        DeviceType,
        fake::{FakeTransport, QueueStatus, State},
    },
};
use alloc::{collections::BTreeMap, rc::Rc, vec};
use core::cell::RefCell;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Ledger {
    allocations: BTreeMap<PhysAddr, (usize, usize, BufferDirection, bool)>,
    mappings: BTreeMap<PhysAddr, (usize, usize, BufferDirection, bool)>,
    allocation_calls: usize,
    share_calls: usize,
    fail_allocate: usize,
    fail_share: usize,
    rejected_allocations: usize,
    rejected_mappings: usize,
    sync_for_device: usize,
    sync_for_cpu: usize,
}

#[derive(Clone)]
struct Backend {
    offset: u64,
    ring_bus_base: Option<u64>,
    mask: u64,
    is_coherent: bool,
    ledger: Rc<RefCell<Ledger>>,
}

impl Backend {
    fn new(offset: u64) -> Self {
        Self {
            offset,
            ring_bus_base: None,
            mask: u64::MAX,
            is_coherent: true,
            ledger: Rc::default(),
        }
    }

    fn assert_empty(&self) {
        let ledger = self.ledger.borrow();
        assert!(ledger.allocations.is_empty());
        assert!(ledger.mappings.is_empty());
    }

    fn accepts(&self, address: u64, len: usize) -> bool {
        len.checked_sub(1)
            .and_then(|last| address.checked_add(last as u64))
            .is_some_and(|last| address & !self.mask == 0 && last & !self.mask == 0)
    }
}

// SAFETY: FakeHal owns the backing allocations and bounce mappings. The ledger
// retains original arguments and checks exact matching releases per instance.
// Synthetic translated addresses are never dereferenced as CPU pointers.
unsafe impl DmaBackend for Backend {
    fn allocate(
        &self,
        pages: usize,
        direction: BufferDirection,
        platform: bool,
    ) -> Result<(PhysAddr, NonNull<u8>)> {
        let mut ledger = self.ledger.borrow_mut();
        ledger.allocation_calls += 1;
        if ledger.allocation_calls == ledger.fail_allocate {
            return Err(Error::DmaError);
        }
        let (physical, pointer) = FakeHal::dma_alloc(pages, direction, platform);
        let address = self.ring_bus_base.map_or(physical + self.offset, |base| {
            base + ledger.allocation_calls as u64 * 0x10000
        });
        if !self.accepts(address, pages * PAGE_SIZE) {
            // SAFETY: This fresh FakeHal allocation was never published and uses its original arguments.
            unsafe {
                FakeHal::dma_dealloc(physical, pointer, pages, platform);
            }
            ledger.rejected_allocations += 1;
            return Err(Error::DmaError);
        }
        assert!(
            ledger
                .allocations
                .insert(
                    address,
                    (pointer.as_ptr() as usize, pages, direction, platform)
                )
                .is_none()
        );
        Ok((address, pointer))
    }

    unsafe fn deallocate(
        &self,
        address: PhysAddr,
        pointer: NonNull<u8>,
        pages: usize,
        direction: BufferDirection,
        platform: bool,
    ) {
        assert_eq!(
            self.ledger.borrow_mut().allocations.remove(&address),
            Some((pointer.as_ptr() as usize, pages, direction, platform))
        );
        // SAFETY: The ledger verified this live allocation's original parameters.
        unsafe {
            FakeHal::dma_dealloc(pointer.as_ptr() as PhysAddr, pointer, pages, platform);
        }
    }

    unsafe fn share(
        &self,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        platform: bool,
    ) -> Result<PhysAddr> {
        let mut ledger = self.ledger.borrow_mut();
        ledger.share_calls += 1;
        if ledger.share_calls == ledger.fail_share {
            return Err(Error::DmaError);
        }
        // SAFETY: The queue supplies valid exclusive buffers, retained until unshare.
        let physical = unsafe { FakeHal::share(buffer, direction, platform) };
        let address = physical + self.offset;
        if !self.accepts(address, buffer.len()) {
            // SAFETY: This fresh mapping has not been exposed to a device; these are its original arguments.
            unsafe {
                FakeHal::unshare(physical, buffer, direction, platform);
            }
            ledger.rejected_mappings += 1;
            return Err(Error::DmaError);
        }
        if !self.is_coherent {
            ledger.sync_for_device += 1;
        }
        assert!(
            ledger
                .mappings
                .insert(
                    address,
                    (
                        buffer.as_ptr() as *mut u8 as usize,
                        buffer.len(),
                        direction,
                        platform
                    )
                )
                .is_none()
        );
        Ok(address)
    }

    unsafe fn unshare(
        &self,
        address: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        platform: bool,
    ) {
        assert_eq!(
            self.ledger.borrow_mut().mappings.remove(&address),
            Some((
                buffer.as_ptr() as *mut u8 as usize,
                buffer.len(),
                direction,
                platform
            ))
        );
        // SAFETY: The ledger verified the original mapping; caller has ended device access.
        unsafe {
            FakeHal::unshare(address - self.offset, buffer, direction, platform);
        }
        if !self.is_coherent {
            self.ledger.borrow_mut().sync_for_cpu += 1;
        }
    }
}

fn transport() -> FakeTransport<()> {
    FakeTransport {
        device_type: DeviceType::EntropySource,
        max_queue_size: 8,
        device_features: 0,
        state: Arc::new(Mutex::new(State::new(vec![QueueStatus::default()], ()))),
    }
}

fn complete(queue: &mut VirtQueue<FakeHal, 8, Backend>, token: u16) {
    let slot = queue.last_used_idx as usize & 7;
    // SAFETY: This test is the only simulated device, and the used ring is live.
    unsafe {
        (*queue.used.as_ptr()).ring[slot] = UsedElem {
            id: token.into(),
            len: 0,
        };
        (*queue.used.as_ptr())
            .idx
            .store(queue.last_used_idx.wrapping_add(1), Ordering::Release);
    }
}

fn mapping_failure_cases(indirect: bool, failure_points: usize) {
    for failure in 1..=failure_points {
        let backend = Backend::new(0x10000);
        backend.ledger.borrow_mut().fail_share = failure;
        let mut transport = transport();
        let mut queue = VirtQueue::<FakeHal, 8, _>::new_with_dma(
            &mut transport,
            0,
            indirect,
            false,
            true,
            backend.clone(),
        )
        .unwrap();
        let input = [1u8; 16];
        let mut output = [0u8; 16];
        // SAFETY: A failed submission publishes nothing; buffers remain live throughout rollback.
        assert!(matches!(
            unsafe { queue.add(&[&input], &mut [&mut output]) },
            Err(Error::DmaError)
        ));
        assert!(backend.ledger.borrow().mappings.is_empty());
        assert_eq!(queue.available_desc(), 8);
        assert_eq!(queue.avail_idx, 0);
        assert_eq!(queue.free_head, 0);
        // SAFETY: The buffers remain live and untouched until simulated completion below.
        let token = unsafe { queue.add(&[&input], &mut [&mut output]) }.unwrap();
        complete(&mut queue, token);
        // SAFETY: The same buffers/token are returned after the simulated device completed.
        unsafe { queue.pop_used(token, &[&input], &mut [&mut output]) }.unwrap();
        assert!(backend.ledger.borrow().mappings.is_empty());
        assert_eq!(queue.available_desc(), 8);
        drop(queue);
        backend.assert_empty();
    }
}

#[test]
fn direct_mapping_failure_rolls_back_and_queue_is_reusable() {
    mapping_failure_cases(false, 2);
}

#[test]
#[cfg(feature = "alloc")]
fn indirect_mapping_failure_including_table_rolls_back() {
    mapping_failure_cases(true, 3);
}

#[test]
fn ring_allocation_failure_does_not_configure_transport() {
    for failure in 1..=2 {
        let backend = Backend::new(0x20000);
        backend.ledger.borrow_mut().fail_allocate = failure;
        let mut transport = transport();
        assert!(matches!(
            VirtQueue::<FakeHal, 8, _>::new_with_dma(
                &mut transport,
                0,
                false,
                false,
                false,
                backend.clone()
            ),
            Err(Error::DmaError)
        ));
        assert!(!transport.queue_used(0));
        backend.assert_empty();
    }
}

#[test]
fn rng_instance_backend_handles_failure_and_successful_completion() {
    use crate::device::rng::VirtIORng;

    let backend = Backend::new(0);
    backend.ledger.borrow_mut().fail_share = 1;
    let transport = transport();
    let state = transport.state.clone();
    let mut rng = VirtIORng::<FakeHal, _, _>::new_with_dma(transport, backend.clone()).unwrap();
    let mut output = [0u8; 16];
    // SAFETY: A failed submission leaves no outstanding access to the live buffer.
    assert!(matches!(
        unsafe { rng.request_entropy_nb(&mut output) },
        Err(Error::DmaError)
    ));
    assert!(backend.ledger.borrow().mappings.is_empty());
    // SAFETY: Output remains live and exclusively device-owned until completion.
    let token = unsafe { rng.request_entropy_nb(&mut output) }.unwrap();
    state.lock().unwrap().write_to_queue::<8>(0, &[37u8; 16]);
    // SAFETY: Exact original buffer and token after the fake device completed.
    let count = unsafe { rng.complete_request_entropy(token, &mut output) }.unwrap();
    assert_eq!(count, 16);
    assert_eq!(output, [37u8; 16]);
    assert!(backend.ledger.borrow().mappings.is_empty());
    drop(rng);
    assert_eq!(state.lock().unwrap().queues[0].size, 0);
    backend.assert_empty();
}

#[test]
fn legacy_and_modern_rings_release_through_original_instances() {
    let first = Backend::new(0x10000);
    let second = Backend::new(0x30000);
    let legacy = VirtQueueLayout::<FakeHal, _>::allocate_legacy(8, false, first.clone()).unwrap();
    let modern = VirtQueueLayout::<FakeHal, _>::allocate_flexible(8, true, second.clone()).unwrap();
    assert_eq!(first.ledger.borrow().allocations.len(), 1);
    assert_eq!(second.ledger.borrow().allocations.len(), 2);
    drop(legacy);
    first.assert_empty();
    assert_eq!(second.ledger.borrow().allocations.len(), 2);
    drop(modern);
    second.assert_empty();
}

#[test]
fn legacy_transport_configures_and_recycles_instance_queue() {
    use crate::transport::mmio::{LEGACY_VERSION, MmioTransport, VirtIOHeader};
    use safe_mmio::UniqueMmioPointer;

    let mut backend = Backend::new(0x20000);
    backend.ring_bus_base = Some(0x100000);
    let mut header = VirtIOHeader::make_fake_header(LEGACY_VERSION, 4, 0, 0, 8);
    let mut transport = MmioTransport::new_from_unique(
        UniqueMmioPointer::from(&mut header),
        UniqueMmioPointer::from([].as_mut_slice()),
    )
    .unwrap();
    let mut queue = VirtQueue::<FakeHal, 8, _>::new_with_dma(
        &mut transport,
        0,
        false,
        false,
        false,
        backend.clone(),
    )
    .unwrap();
    assert!(transport.queue_used(0));
    assert_eq!(queue.layout.descriptors_paddr(), 0x110000);
    assert_eq!(backend.ledger.borrow().allocations.len(), 1);
    let mut output = [0u8; 16];
    // SAFETY: Output is retained until the simulated completion below.
    let token = unsafe { queue.add(&[], &mut [&mut output]) }.unwrap();
    complete(&mut queue, token);
    // SAFETY: This is the original buffer and token after completion.
    unsafe { queue.pop_used(token, &[], &mut [&mut output]) }.unwrap();
    transport.queue_unset(0);
    assert!(!transport.queue_used(0));
    drop(queue);
    backend.assert_empty();
}

#[test]
fn concurrent_instances_do_not_cross_mapping_domains() {
    for indirect in [false, cfg!(feature = "alloc")] {
        let first = Backend::new(0x10000);
        let mut second = Backend::new(0x30000);
        second.is_coherent = false;
        let mut first_transport = transport();
        let mut second_transport = transport();
        let mut first_queue = VirtQueue::<FakeHal, 8, _>::new_with_dma(
            &mut first_transport,
            0,
            indirect,
            false,
            false,
            first.clone(),
        )
        .unwrap();
        let mut second_queue = VirtQueue::<FakeHal, 8, _>::new_with_dma(
            &mut second_transport,
            0,
            indirect,
            false,
            true,
            second.clone(),
        )
        .unwrap();
        let input = [1u8; 16];
        let mut first_output = [0u8; 16];
        let mut second_output = [0u8; 16];
        assert_eq!(
            first_queue.layout.descriptors_paddr(),
            first_queue.layout.descriptors_vaddr().as_ptr() as u64 + first.offset
        );
        assert_eq!(
            second_queue.layout.descriptors_paddr(),
            second_queue.layout.descriptors_vaddr().as_ptr() as u64 + second.offset
        );
        // SAFETY: Both submissions keep their buffers alive until completion; input is read-only.
        let first_token = unsafe { first_queue.add(&[&input], &mut [&mut first_output]) }.unwrap();
        // SAFETY: Second output is disjoint and retained until its own completion.
        let second_token =
            unsafe { second_queue.add(&[&input], &mut [&mut second_output]) }.unwrap();
        let expected = if indirect { 3 } else { 2 };
        assert_eq!(first.ledger.borrow().mappings.len(), expected);
        assert_eq!(second.ledger.borrow().mappings.len(), expected);
        assert!(
            first
                .ledger
                .borrow()
                .mappings
                .contains_key(&first_queue.desc_shadow[usize::from(first_token)].addr)
        );
        assert!(
            second
                .ledger
                .borrow()
                .mappings
                .contains_key(&second_queue.desc_shadow[usize::from(second_token)].addr)
        );
        assert_eq!(first.ledger.borrow().sync_for_device, 0);
        assert_eq!(second.ledger.borrow().sync_for_device, expected);
        assert_eq!(second.ledger.borrow().sync_for_cpu, 0);
        complete(&mut first_queue, first_token);
        // SAFETY: Exact original buffers after simulated first-device completion.
        unsafe { first_queue.pop_used(first_token, &[&input], &mut [&mut first_output]) }.unwrap();
        drop(first_queue);
        first.assert_empty();
        assert_eq!(second.ledger.borrow().mappings.len(), expected);
        complete(&mut second_queue, second_token);
        // SAFETY: Exact original buffers after simulated second-device completion.
        unsafe { second_queue.pop_used(second_token, &[&input], &mut [&mut second_output]) }
            .unwrap();
        drop(second_queue);
        second.assert_empty();
        assert_eq!(first.ledger.borrow().sync_for_cpu, 0);
        assert_eq!(second.ledger.borrow().sync_for_cpu, expected);
    }
}

#[test]
fn device_mask_rejection_does_not_change_the_other_instance() {
    let mut restricted = Backend::new(0x200000);
    restricted.ring_bus_base = Some(0x100000);
    restricted.mask = 0x1fffff;
    let unrestricted = Backend::new(0x30000);
    let mut restricted_transport = transport();
    let mut unrestricted_transport = transport();
    let mut restricted_queue = VirtQueue::<FakeHal, 8, _>::new_with_dma(
        &mut restricted_transport,
        0,
        false,
        false,
        false,
        restricted.clone(),
    )
    .unwrap();
    let mut unrestricted_queue = VirtQueue::<FakeHal, 8, _>::new_with_dma(
        &mut unrestricted_transport,
        0,
        false,
        false,
        false,
        unrestricted.clone(),
    )
    .unwrap();
    let mut rejected_output = [0u8; 16];
    let mut accepted_output = [0u8; 16];
    // SAFETY: The rejected mapping publishes nothing; the buffer stays live during rollback.
    assert!(matches!(
        unsafe { restricted_queue.add(&[], &mut [&mut rejected_output]) },
        Err(Error::DmaError)
    ));
    assert_eq!(restricted.ledger.borrow().rejected_mappings, 1);
    assert!(restricted.ledger.borrow().mappings.is_empty());
    assert_eq!(restricted_queue.avail_idx, 0);
    assert_eq!(restricted_queue.available_desc(), 8);
    // SAFETY: The accepted buffer stays live until the matching completion below.
    let token = unsafe { unrestricted_queue.add(&[], &mut [&mut accepted_output]) }.unwrap();
    complete(&mut unrestricted_queue, token);
    // SAFETY: This is the original buffer/token after simulated device completion.
    unsafe { unrestricted_queue.pop_used(token, &[], &mut [&mut accepted_output]) }.unwrap();
    assert_eq!(unrestricted.ledger.borrow().rejected_mappings, 0);
    drop(restricted_queue);
    drop(unrestricted_queue);
    restricted.assert_empty();
    unrestricted.assert_empty();
}

#[test]
fn ring_mask_checks_the_entire_allocation_and_cleans_up() {
    let mut backend = Backend::new(0);
    backend.ring_bus_base = Some(0x1ef000);
    backend.mask = 0x1fffff;
    assert!(backend.accepts(0x1ff000, PAGE_SIZE));
    assert!(!backend.accepts(0x1ff000, PAGE_SIZE * 2));
    assert!(matches!(
        VirtQueueLayout::<FakeHal, _>::allocate_legacy(8, false, backend.clone()),
        Err(Error::DmaError)
    ));
    assert_eq!(backend.ledger.borrow().rejected_allocations, 1);
    backend.assert_empty();
}

#[test]
fn rng_stop_waits_for_reset_before_canceling_without_completion() {
    use crate::{device::rng::VirtIORng, transport::DeviceStatus};

    let backend = Backend::new(0);
    let transport = transport();
    let state = transport.state.clone();
    let mut rng = VirtIORng::<FakeHal, _, _>::new_with_dma(transport, backend.clone()).unwrap();
    let mut output = [0u8; 16];
    assert!(!rng.poll_stopped());
    // SAFETY: The output remains alive and inaccessible until confirmed reset and cancellation.
    let token = unsafe { rng.request_entropy_nb(&mut output) }.unwrap();
    rng.begin_stop();
    state.lock().unwrap().status = DeviceStatus::DRIVER_OK;
    rng.begin_stop();
    assert!(!rng.poll_stopped());
    // SAFETY: Original live token and buffer; reset is pending so this must leave the mapping intact.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(token, &mut output) },
        Err(Error::NotReady)
    );
    assert_eq!(backend.ledger.borrow().mappings.len(), 1);
    assert_eq!(backend.ledger.borrow().allocations.len(), 2);
    let mut unused = [0u8; 4];
    assert_eq!(rng.request_entropy(&mut unused), Err(Error::NotReady));
    // SAFETY: New live buffer; the stopped gate must reject it without publishing it.
    assert_eq!(
        unsafe { rng.request_entropy_nb(&mut unused) },
        Err(Error::NotReady)
    );
    state.lock().unwrap().interrupt_pending = true;
    assert!(rng.ack_interrupt().is_empty());
    assert!(state.lock().unwrap().interrupt_pending);
    state.lock().unwrap().status = DeviceStatus::empty();
    assert!(rng.poll_stopped());
    assert!(rng.poll_stopped());
    // SAFETY: Original live token/buffer; simulated reset has completed and no IRQ worker exists.
    unsafe { rng.cancel_request_entropy(token, &mut output) }.unwrap();
    assert!(backend.ledger.borrow().mappings.is_empty());
    // SAFETY: Buffer remains valid; a duplicate token must be rejected before touching it.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(token, &mut output) },
        Err(Error::WrongToken)
    );
    drop(rng);
    backend.assert_empty();
}

#[test]
fn rng_stop_obeys_transport_completion_not_raw_status() {
    use crate::device::rng::VirtIORng;

    let backend = Backend::new(0);
    let transport = transport();
    let state = transport.state.clone();
    let mut rng = VirtIORng::<FakeHal, _, _>::new_with_dma(transport, backend.clone()).unwrap();
    let mut output = [0u8; 16];
    assert!(!rng.poll_stopped());
    assert_eq!(state.lock().unwrap().reset_polls, 0);
    // SAFETY: Output stays alive and exclusive until reset confirmation and cancellation below.
    let token = unsafe { rng.request_entropy_nb(&mut output) }.unwrap();
    state.lock().unwrap().is_reset_pending = true;
    rng.begin_stop();
    rng.begin_stop();
    assert_eq!(state.lock().unwrap().reset_requests, 1);
    assert!(state.lock().unwrap().status.is_empty());
    assert!(!rng.poll_stopped());
    // SAFETY: Original live token/buffer; pending transport completion must prevent reclamation.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(token, &mut output) },
        Err(Error::NotReady)
    );
    assert_eq!(backend.ledger.borrow().mappings.len(), 1);
    assert_eq!(backend.ledger.borrow().allocations.len(), 2);
    state.lock().unwrap().is_reset_pending = false;
    assert!(rng.poll_stopped());
    let polls = state.lock().unwrap().reset_polls;
    rng.begin_stop();
    assert!(rng.poll_stopped());
    assert_eq!(state.lock().unwrap().reset_requests, 1);
    assert_eq!(state.lock().unwrap().reset_polls, polls);
    assert_eq!(rng.request_entropy(&mut []), Err(Error::NotReady));
    // SAFETY: Original live buffer, confirmed simulated reset, and no concurrent software users.
    unsafe { rng.cancel_request_entropy(token, &mut output) }.unwrap();
    drop(rng);
    backend.assert_empty();
}

#[test]
fn rng_completed_request_is_not_reclaimed_again_during_stop() {
    use crate::device::rng::VirtIORng;

    let backend = Backend::new(0);
    let transport = transport();
    let state = transport.state.clone();
    let mut rng = VirtIORng::<FakeHal, _, _>::new_with_dma(transport, backend.clone()).unwrap();
    let mut output = [0u8; 16];
    // SAFETY: Output remains exclusive until the matching completion below.
    let token = unsafe { rng.request_entropy_nb(&mut output) }.unwrap();
    state.lock().unwrap().write_to_queue::<8>(0, &[11u8; 16]);
    // SAFETY: Original token and buffer after simulated completion.
    assert_eq!(
        unsafe { rng.complete_request_entropy(token, &mut output) },
        Ok(16)
    );
    rng.begin_stop();
    assert!(rng.poll_stopped());
    // SAFETY: A completed token is rejected before any mapping operation.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(token, &mut output) },
        Err(Error::WrongToken)
    );
    assert!(backend.ledger.borrow().mappings.is_empty());
    drop(rng);
    backend.assert_empty();
}

#[test]
fn rng_stop_cancels_an_unconsumed_used_entry_without_reporting_entropy() {
    use crate::device::rng::VirtIORng;

    let backend = Backend::new(0);
    let transport = transport();
    let state = transport.state.clone();
    let mut rng = VirtIORng::<FakeHal, _, _>::new_with_dma(transport, backend.clone()).unwrap();
    let mut output = [0u8; 16];
    // SAFETY: Output remains alive and exclusive until cancellation after reset.
    let token = unsafe { rng.request_entropy_nb(&mut output) }.unwrap();
    state.lock().unwrap().write_to_queue::<8>(0, &[23u8; 16]);
    rng.begin_stop();
    assert!(rng.poll_stopped());
    // SAFETY: Out-of-range tokens are rejected without accessing the live buffer.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(u16::MAX, &mut output) },
        Err(Error::WrongToken)
    );
    assert_eq!(backend.ledger.borrow().mappings.len(), 1);
    // SAFETY: Original live buffer and token; simulated reset is complete with no IRQ worker.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(token, &mut output) },
        Ok(())
    );
    assert!(backend.ledger.borrow().mappings.is_empty());
    // SAFETY: Stop rejects completion without accessing the stale token or buffer.
    assert_eq!(
        unsafe { rng.complete_request_entropy(token, &mut output) },
        Err(Error::NotReady)
    );
    // SAFETY: Duplicate cancellation rejects the token before any buffer or mapping access.
    assert_eq!(
        unsafe { rng.cancel_request_entropy(token, &mut output) },
        Err(Error::WrongToken)
    );
    drop(rng);
    backend.assert_empty();
}

#[test]
fn rng_stop_reclaims_the_current_submission_after_token_reuse() {
    use crate::device::rng::VirtIORng;

    let backend = Backend::new(0);
    let transport = transport();
    let state = transport.state.clone();
    let mut rng = VirtIORng::<FakeHal, _, _>::new_with_dma(transport, backend.clone()).unwrap();
    let mut first = [0u8; 8];
    let mut second = [0u8; 16];
    // SAFETY: First buffer is held until its matching device completion.
    let old_token = unsafe { rng.request_entropy_nb(&mut first) }.unwrap();
    state.lock().unwrap().write_to_queue::<8>(0, &[5u8; 8]);
    // SAFETY: Exact first submission after simulated completion.
    unsafe { rng.complete_request_entropy(old_token, &mut first) }.unwrap();
    // SAFETY: Second buffer is held until reset and cancellation below.
    let current_token = unsafe { rng.request_entropy_nb(&mut second) }.unwrap();
    assert_eq!(current_token, old_token);
    rng.begin_stop();
    assert!(rng.poll_stopped());
    assert_eq!(backend.ledger.borrow().mappings.len(), 1);
    // SAFETY: Only the current submission's buffer is valid for this reused token.
    unsafe { rng.cancel_request_entropy(current_token, &mut second) }.unwrap();
    drop(rng);
    backend.assert_empty();
}
