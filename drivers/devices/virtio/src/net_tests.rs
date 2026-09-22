// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};
use virtio_drivers::transport::DeviceType;

use super::{Features, RawNet};
use crate::mock_virtio::{MockHal, MockTransport};

const QUEUE_SIZE: usize = 64;

struct RxFixture {
    // Drop the transport and queues before their packet backing storage.
    net: RawNet<MockHal, MockTransport, QUEUE_SIZE>,
    buffers: Vec<Vec<u8>>,
    avail_addr: usize,
    used_addr: usize,
    next_used: u16,
    notifications: Arc<[AtomicUsize; 2]>,
}

impl RxFixture {
    fn new(has_event_idx: bool) -> Self {
        let mut transport = MockTransport::new_with_type(DeviceType::Network);
        transport.queue_size = QUEUE_SIZE as u32;
        transport.features = Features::MAC.bits() | Features::VERSION_1.bits();
        if has_event_idx {
            transport.features |= Features::RING_EVENT_IDX.bits();
        }
        let addresses = transport.queue_addresses.clone();
        let notifications = transport.notify_count.clone();
        let mut net = RawNet::new(transport).unwrap();
        let mut buffers: Vec<Vec<u8>> = (0..QUEUE_SIZE).map(|_| vec![0; 1526]).collect();
        for buffer in &mut buffers {
            // SAFETY: each buffer stays live in the fixture until its token is
            // completed; device-owned contents are only accessed at completion.
            unsafe { net.receive_begin(buffer).unwrap() };
        }
        net.flush_receive_notify();
        let [_, avail_addr, used_addr] = addresses.lock()[0];
        Self {
            net,
            buffers,
            avail_addr: avail_addr as usize,
            used_addr: used_addr as usize,
            next_used: 0,
            notifications,
        }
    }

    fn avail_idx(&self) -> u16 {
        // SAFETY: MockHal uses identity-mapped DMA; the live RX available ring
        // has its aligned u16 index at offset 2. This fixture is single-threaded.
        unsafe { ((self.avail_addr + 2) as *const u16).read_volatile() }
    }

    fn request_event(&self, index: u16) {
        // SAFETY: the live split used ring stores avail_event after its header
        // and QUEUE_SIZE eight-byte used entries. The offset is u16-aligned.
        unsafe {
            ((self.used_addr + 4 + QUEUE_SIZE * 8) as *mut u16).write_volatile(index);
        }
    }

    fn suppress_notifications(&self, is_suppressed: bool) {
        // SAFETY: the live used ring starts with an aligned u16 flags field;
        // only this single-threaded fake device writes it.
        unsafe { (self.used_addr as *mut u16).write_volatile(u16::from(is_suppressed)) };
    }

    fn device_complete(&mut self) -> u16 {
        core::assert!(self.next_used != self.avail_idx());
        let slot = usize::from(self.next_used) % QUEUE_SIZE;
        // SAFETY: both rings belong to the live queue, all offsets follow the
        // split-ring ABI and slot is bounded. Only this fake device writes the
        // used ring. Buffers are initialized; 64 is a valid completion length.
        unsafe {
            let token = ((self.avail_addr + 4 + slot * 2) as *const u16).read_volatile();
            ((self.used_addr + 4 + slot * 8) as *mut u32).write_volatile(u32::from(token));
            ((self.used_addr + 8 + slot * 8) as *mut u32).write_volatile(64);
            self.next_used = self.next_used.wrapping_add(1);
            ((self.used_addr + 2) as *mut u16).write_volatile(self.next_used);
            token
        }
    }

    fn reclaim(&mut self, token: u16) {
        core::assert_eq!(self.net.poll_receive(), Some(token));
        // SAFETY: the fake device completed this token, and the array retains
        // exactly the buffer submitted for it.
        unsafe {
            self.net
                .receive_complete(token, &mut self.buffers[usize::from(token)])
                .unwrap();
        }
    }

    fn refill(&mut self, token: u16) {
        // SAFETY: the token was reclaimed and this buffer stays live and
        // untouched until the next completion. The sole freed descriptor is
        // reused, preserving the fixture's token-to-buffer mapping.
        let new_token = unsafe {
            self.net
                .receive_begin(&mut self.buffers[usize::from(token)])
                .unwrap()
        };
        core::assert_eq!(new_token, token);
    }

    fn reset_notifications(&self) {
        self.notifications[0].store(0, Ordering::Relaxed);
    }

    fn notification_count(&self) -> usize {
        self.notifications[0].load(Ordering::Relaxed)
    }
}

impl Drop for RxFixture {
    fn drop(&mut self) {
        while self.next_used != self.avail_idx() {
            let token = self.device_complete();
            self.reclaim(token);
        }
    }
}

#[def_test]
fn receive_notifications_survive_index_wraparound() {
    let mut fixture = RxFixture::new(true);
    for _ in 0..(65528 - QUEUE_SIZE) {
        let token = fixture.device_complete();
        fixture.request_event(fixture.avail_idx());
        fixture.reclaim(token);
        fixture.refill(token);
        fixture.net.flush_receive_notify();
    }
    assert_eq!(fixture.avail_idx(), 65528);

    // The device exhausts the queue, requests the next available entry and
    // sleeps. The CPU refills completed buffers one by one, as net.rs does.
    let tokens = core::array::from_fn::<_, QUEUE_SIZE, _>(|_| fixture.device_complete());
    fixture.request_event(65528);
    fixture.reset_notifications();
    for token in tokens {
        fixture.reclaim(token);
        fixture.refill(token);
    }
    fixture.net.flush_receive_notify();
    assert_eq!(fixture.avail_idx(), 56);
    assert!(fixture.notification_count() > 0);
    assert!(fixture.notification_count() <= QUEUE_SIZE / 16);
}

#[def_test]
fn receive_flush_preserves_requested_short_batch_notifications() {
    let mut fixture = RxFixture::new(false);
    fixture.reset_notifications();
    fixture.suppress_notifications(true);
    let token = fixture.device_complete();
    fixture.reclaim(token);
    fixture.refill(token);
    fixture.net.flush_receive_notify();
    assert_eq!(fixture.notification_count(), 0);

    fixture.suppress_notifications(false);
    let token = fixture.device_complete();
    fixture.reclaim(token);
    fixture.refill(token);
    assert_eq!(fixture.notification_count(), 0);
    // Later device suppression cannot erase a request already observed by
    // a publication in this batch. An extra notification is permitted.
    fixture.suppress_notifications(true);
    fixture.net.flush_receive_notify();
    assert_eq!(fixture.notification_count(), 1);
    fixture.net.flush_receive_notify();
    assert_eq!(fixture.notification_count(), 1);
}
