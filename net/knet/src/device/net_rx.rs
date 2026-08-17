// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ethernet RX softirq poll sources.
//!
//! Kept separate from `EthernetDevice` so IRQ-safe scheduling does not share a
//! file with ARP/NDP transmit and neighbor resolution.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kclass::prelude::*;
use kpoll::PollSet;
use kspin::SpinNoIrq;
#[cfg(not(unittest))]
use lazyinit::LazyInit;

const NET_RX_SOFTIRQ_BATCH: usize = 8;

static NEXT_NET_RX_SOURCE_ID: AtomicUsize = AtomicUsize::new(1);
static NET_RX_SOURCES: SpinNoIrq<NetRxSources> = SpinNoIrq::new(NetRxSources::new());
#[cfg(not(unittest))]
static NET_RX_SOFTIRQ_INIT: LazyInit<()> = LazyInit::new();
static NET_RX_SOFTIRQ_AVAILABLE: AtomicBool = AtomicBool::new(false);

struct NetRxSources {
    entries: Vec<Arc<NetRxPollSource>>,
    scan_cursor: usize,
}

impl NetRxSources {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            scan_cursor: 0,
        }
    }

    fn push(&mut self, source: Arc<NetRxPollSource>) {
        self.entries.push(source);
    }

    fn remove(&mut self, id: usize) -> Option<Arc<NetRxPollSource>> {
        let removed = self
            .entries
            .iter()
            .position(|source| source.id == id)
            .map(|index| self.entries.swap_remove(index));
        self.normalize_cursor();
        removed
    }

    fn collect_scheduled_fallback_batch(
        &mut self,
        wake_batch: &mut [Option<PollSet>; NET_RX_SOFTIRQ_BATCH],
    ) -> (usize, bool) {
        let source_count = self.entries.len();
        if source_count == 0 {
            self.scan_cursor = 0;
            return (0, false);
        }

        self.scan_cursor %= source_count;
        let start = self.scan_cursor;
        let mut next_cursor = start;
        let mut wake_count = 0;
        let mut has_deferred_pending = false;

        for offset in 0..source_count {
            if wake_count == NET_RX_SOFTIRQ_BATCH {
                has_deferred_pending = self.has_pending_from(start, offset);
                break;
            }

            let index = (start + offset) % source_count;
            let source = &self.entries[index];
            if source.take_pending() {
                wake_batch[wake_count] = Some(source.waiters_clone());
                wake_count += 1;
                next_cursor = (index + 1) % source_count;
            }
        }

        self.scan_cursor = next_cursor;
        (wake_count, has_deferred_pending)
    }

    fn has_pending_from(&self, start: usize, offset_start: usize) -> bool {
        (offset_start..self.entries.len()).any(|offset| {
            let index = (start + offset) % self.entries.len();
            self.entries[index].has_pending()
        })
    }

    fn normalize_cursor(&mut self) {
        if self.entries.is_empty() {
            self.scan_cursor = 0;
        } else if self.scan_cursor >= self.entries.len() {
            self.scan_cursor %= self.entries.len();
        }
    }
}

pub(super) struct NetRxPollSource {
    id: usize,
    pending: AtomicBool,
    waiters: PollSet,
}

impl NetRxPollSource {
    fn new(id: usize) -> Self {
        Self {
            id,
            pending: AtomicBool::new(false),
            waiters: PollSet::new(),
        }
    }

    pub(super) fn id(&self) -> usize {
        self.id
    }

    pub(super) fn waiters(&self) -> &PollSet {
        &self.waiters
    }

    pub(super) fn waiters_clone(&self) -> PollSet {
        self.waiters.clone()
    }

    fn schedule(&self) {
        self.pending.store(true, Ordering::Release);
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::NetRx);
    }

    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
}

pub(super) struct KnetRxScheduler {
    pub(super) source: Weak<NetRxPollSource>,
}

impl NetRxScheduler for KnetRxScheduler {
    fn schedule_rx(&self) {
        if let Some(source) = self.source.upgrade() {
            source.schedule();
        }
    }
}

#[cfg(not(unittest))]
fn ensure_net_rx_softirq_available() -> bool {
    NET_RX_SOFTIRQ_INIT.call_once(|| {
        if kirq::softirq::open_softirq(kirq::softirq::SoftirqVec::NetRx, run_net_rx_softirq) {
            NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        } else {
            warn!("knet: NetRx softirq vector already registered");
        }
    });
    NET_RX_SOFTIRQ_AVAILABLE.load(Ordering::Acquire)
}

#[cfg(unittest)]
fn ensure_net_rx_softirq_available() -> bool {
    let vec = kirq::softirq::SoftirqVec::NetRx;
    if kirq::softirq::softirq_action_matches_for_tests(vec, run_net_rx_softirq) {
        NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        return true;
    }
    if kirq::softirq::is_softirq_open(vec) {
        NET_RX_SOFTIRQ_AVAILABLE.store(false, Ordering::Release);
        return false;
    }

    if kirq::softirq::open_softirq(vec, run_net_rx_softirq) {
        NET_RX_SOFTIRQ_AVAILABLE.store(true, Ordering::Release);
        true
    } else {
        let available = kirq::softirq::softirq_action_matches_for_tests(vec, run_net_rx_softirq);
        NET_RX_SOFTIRQ_AVAILABLE.store(available, Ordering::Release);
        available
    }
}

pub(super) fn register_net_rx_source() -> Option<Arc<NetRxPollSource>> {
    if !ensure_net_rx_softirq_available() {
        return None;
    }
    let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
    let source = Arc::new(NetRxPollSource::new(id));
    NET_RX_SOURCES.lock().push(source.clone());
    Some(source)
}

pub(super) fn unregister_net_rx_source(id: usize) {
    let removed = {
        let mut sources = NET_RX_SOURCES.lock();
        sources.remove(id)
    };
    if let Some(source) = removed {
        source.waiters.wake();
    }
}

fn run_net_rx_softirq() {
    let mut wake_batch: [Option<PollSet>; NET_RX_SOFTIRQ_BATCH] = core::array::from_fn(|_| None);
    let (_, has_deferred_pending) = NET_RX_SOURCES
        .lock()
        .collect_scheduled_fallback_batch(&mut wake_batch);

    for waiters in wake_batch.into_iter().flatten() {
        waiters.wake();
    }

    if has_deferred_pending {
        kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::NetRx);
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable, Waker},
    };

    use unittest::{assert_eq, def_test};

    use super::*;

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn waker_wake(data: *const ()) {
        // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_drop(_data: *const ()) {}

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

    fn new_counter() -> &'static AtomicUsize {
        Box::leak(Box::new(AtomicUsize::new(0)))
    }

    fn make_waker(counter: &'static AtomicUsize) -> Waker {
        let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
        // SAFETY: the raw waker data pointer is the leaked `AtomicUsize` above
        // and the vtable only performs atomic increments on that allocation.
        unsafe { Waker::from_raw(raw) }
    }

    fn test_source(sources: &mut NetRxSources, id: usize) -> Arc<NetRxPollSource> {
        let source = Arc::new(NetRxPollSource::new(id));
        sources.push(source.clone());
        source
    }

    fn collect_and_wake_test_sources(sources: &mut NetRxSources) -> (usize, bool) {
        let mut wake_batch: [Option<PollSet>; NET_RX_SOFTIRQ_BATCH] =
            core::array::from_fn(|_| None);
        let result = sources.collect_scheduled_fallback_batch(&mut wake_batch);
        for waiters in wake_batch.into_iter().flatten() {
            waiters.wake();
        }
        result
    }

    #[def_test(serial)]
    fn test_net_rx_softirq_wakes_only_pending_sources() {
        let mut sources = NetRxSources::new();
        let pending_id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let idle_id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let pending_source = test_source(&mut sources, pending_id);
        let idle_source = test_source(&mut sources, idle_id);
        let pending_counter = new_counter();
        let idle_counter = new_counter();
        let pending_registration = pending_source
            .waiters()
            .register(&make_waker(pending_counter))
            .unwrap();
        let idle_registration = idle_source
            .waiters()
            .register(&make_waker(idle_counter))
            .unwrap();

        pending_source.pending.store(true, Ordering::Release);
        assert_eq!(collect_and_wake_test_sources(&mut sources), (1, false));

        assert_eq!(pending_counter.load(Ordering::SeqCst), 1);
        assert_eq!(idle_counter.load(Ordering::SeqCst), 0);

        drop(pending_registration);
        drop(idle_registration);
    }

    #[def_test(serial)]
    fn test_net_rx_softirq_round_robin_cursor_reaches_sources_after_batch() {
        let mut local_sources = NetRxSources::new();
        let mut sources = Vec::new();
        let mut counters = Vec::new();
        let mut registrations = Vec::new();
        for _ in 0..(NET_RX_SOFTIRQ_BATCH + 2) {
            let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
            let source = test_source(&mut local_sources, id);
            let counter = new_counter();
            registrations.push(source.waiters().register(&make_waker(counter)).unwrap());
            counters.push(counter);
            sources.push(source);
        }

        for source in &sources {
            source.pending.store(true, Ordering::Release);
        }

        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (NET_RX_SOFTIRQ_BATCH, true)
        );
        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (2, false)
        );

        for counter in &counters {
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }

        drop(registrations);
    }

    #[def_test(serial)]
    fn test_net_rx_softirq_full_batch_without_remaining_pending_does_not_defer() {
        let mut local_sources = NetRxSources::new();
        let mut sources = Vec::new();
        let mut counters = Vec::new();
        let mut registrations = Vec::new();
        for _ in 0..(NET_RX_SOFTIRQ_BATCH + 2) {
            let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
            let source = test_source(&mut local_sources, id);
            let counter = new_counter();
            registrations.push(source.waiters().register(&make_waker(counter)).unwrap());
            counters.push(counter);
            sources.push(source);
        }

        for source in sources.iter().take(NET_RX_SOFTIRQ_BATCH) {
            source.pending.store(true, Ordering::Release);
        }

        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (NET_RX_SOFTIRQ_BATCH, false)
        );
        assert_eq!(
            collect_and_wake_test_sources(&mut local_sources),
            (0, false)
        );

        for counter in counters.iter().take(NET_RX_SOFTIRQ_BATCH) {
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        for counter in counters.iter().skip(NET_RX_SOFTIRQ_BATCH) {
            assert_eq!(counter.load(Ordering::SeqCst), 0);
        }

        drop(registrations);
    }
}
