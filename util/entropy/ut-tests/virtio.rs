// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real-work collector handoff with isolated registries and controlled sources.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcompletion::WaitQueue;
use kspin::SpinNoIrq;
use ktask::completion::CompletionWait;
use subsys_hwrng::{HwrngDevice, HwrngRegistration, KError, KResult, completion::RngDrain};
use unittest::{assert, assert_eq, def_test};

use super::*;

static MIXES: AtomicUsize = AtomicUsize::new(0);

fn mix_sample(_: &[u8]) {
    MIXES.fetch_add(1, Ordering::Release);
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = khal::time::monotonic_time_nanos().saturating_add(5_000_000_000);
    loop {
        if condition() {
            return true;
        }
        if khal::time::monotonic_time_nanos() >= deadline {
            return false;
        }
        ktask::yield_now();
    }
}

struct Drain(WaitQueue);

impl RngDrain for Drain {
    fn wait_until(&self, predicate: &mut dyn FnMut() -> bool) -> KResult {
        let wait = CompletionWait::prepare().map_err(|_| KError::InvalidInput)?;
        wait.wait_until(&self.0, predicate);
        Ok(())
    }

    fn notify(&self) {
        self.0.notify_all();
    }
}

#[derive(Clone, Copy)]
enum Response {
    Ready,
    Blocked,
    Retry(KError, usize),
    Failed,
    Invalid,
}

struct Hardware {
    response: SpinNoIrq<Response>,
    reads: AtomicUsize,
    ready: WaitQueue,
}

impl Hardware {
    fn stop(&self) {
        *self.response.lock() = Response::Failed;
        self.ready.notify_all();
    }
}

impl HwrngDevice for Hardware {
    fn read(&self, bytes: &mut [u8]) -> KResult<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.reads.fetch_add(1, Ordering::Release);
        let wait = CompletionWait::prepare().map_err(|_| KError::InvalidInput)?;
        wait.wait_until(&self.ready, &mut || {
            !matches!(*self.response.lock(), Response::Blocked)
        });
        let mut response = self.response.lock();
        match *response {
            Response::Ready => {
                bytes.fill(0x5a);
                Ok(bytes.len())
            }
            Response::Retry(error, remaining) => {
                *response = if remaining > 1 {
                    Response::Retry(error, remaining - 1)
                } else {
                    Response::Ready
                };
                Err(error)
            }
            Response::Invalid => Ok(0),
            Response::Failed | Response::Blocked => Err(KError::Io),
        }
    }

    fn is_failed(&self) -> bool {
        matches!(*self.response.lock(), Response::Failed)
    }
}

struct Provider {
    registration: HwrngRegistration,
    hardware: Arc<Hardware>,
}

impl Provider {
    fn unregister(&mut self) {
        self.hardware.stop();
        self.registration.unregister().unwrap();
    }
}

struct Fixture {
    registry: &'static HwrngRegistry,
    collection: Arc<EntropyCollection>,
    providers: Vec<Provider>,
    is_finished: bool,
}

impl Fixture {
    fn new(registry: &'static HwrngRegistry) -> Self {
        MIXES.store(0, Ordering::Release);
        let collection = EntropyCollection::new(registry, mix_sample).unwrap();
        collection.work.flush().unwrap();
        Self {
            registry,
            collection,
            providers: Vec::new(),
            is_finished: false,
        }
    }

    fn add(&mut self, name: &'static str, response: Response) -> usize {
        let hardware = Arc::new(Hardware {
            response: SpinNoIrq::new(response),
            reads: AtomicUsize::new(0),
            ready: WaitQueue::new(),
        });
        let registration = self
            .registry
            .attach_legacy(
                name,
                "test-rng".into(),
                hardware.clone(),
                Arc::new(Drain(WaitQueue::new())),
            )
            .unwrap();
        self.providers.push(Provider {
            registration,
            hardware,
        });
        self.providers.len() - 1
    }

    fn selected(&self, index: usize) -> bool {
        let state = self.collection.collector.state.lock();
        state.selected.as_ref().is_some_and(|selected| {
            selected.same_publication(&self.providers[index].registration.publication())
        })
    }

    fn finish(&mut self) {
        if self.is_finished {
            return;
        }
        for provider in &self.providers {
            provider.hardware.stop();
        }
        for provider in &mut self.providers {
            provider.unregister();
        }
        self.collection.work.flush().unwrap();
        self.collection.work.shutdown().unwrap();
        self.is_finished = true;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.finish();
    }
}

struct CollectionOverride(Option<Arc<EntropyCollection>>);

impl Drop for CollectionOverride {
    fn drop(&mut self) {
        let test_collection = core::mem::replace(&mut *COLLECTION.lock(), self.0.take());
        drop(test_collection);
    }
}

#[def_test(serial)]
fn source_entry_retains_initial_demand_when_no_device_is_available() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    let saved = COLLECTION.lock().replace(fixture.collection.clone());
    let restore = CollectionOverride(saved);
    let absent = !is_present();
    let _samples = crate::source::read_all_available(32);
    drop(restore);
    fixture.collection.work.flush().unwrap();
    let retained = fixture.collection.collector.state.lock().has_demand;
    fixture.add("virtio-rng-entry", Response::Ready);
    let resumed = wait_for(|| MIXES.load(Ordering::Acquire) == 1);
    fixture.finish();
    assert!(absent && retained && resumed);
}

#[def_test]
fn test_is_virtio_rng_name_matching() {
    assert!(is_virtio_rng("virtio-rng"));
    assert!(is_virtio_rng("platform-virtio-rng-mmio"));
    assert!(is_virtio_rng("platform-virtio-rng-pci"));
    assert!(!is_virtio_rng("unknown"));
    assert!(!is_virtio_rng("uart"));
    assert!(!is_virtio_rng("virtio-blk"));
}

#[def_test(serial)]
fn idle_removal_selects_existing_sibling_and_ignores_late_old_event() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    let first = fixture.add("virtio-rng-a", Response::Ready);
    fixture.collection.work.flush().unwrap();
    let second = fixture.add("virtio-rng-b", Response::Ready);
    fixture.collection.work.flush().unwrap();
    let kept_first = fixture.selected(first);
    let old = fixture.providers[first].registration.publication();
    fixture.providers[first].unregister();
    fixture.collection.work.flush().unwrap();
    let switched = fixture.selected(second);
    fixture.collection.note_change(old);
    fixture.collection.work.flush().unwrap();
    let kept_second = fixture.selected(second);
    let reads = fixture.providers[second]
        .hardware
        .reads
        .load(Ordering::Acquire);
    fixture.finish();
    assert!(kept_first && switched && kept_second);
    assert_eq!(reads, 0);
}

#[def_test(serial)]
fn in_flight_withdrawal_transfers_demand_without_a_new_request_or_event() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    let first = fixture.add("virtio-rng-a", Response::Blocked);
    fixture.collection.request();
    let entered = wait_for(|| {
        fixture.providers[first]
            .hardware
            .reads
            .load(Ordering::Acquire)
            == 1
    });
    let second = fixture.add("virtio-rng-b", Response::Ready);
    fixture.providers[first].unregister();
    let mixed = wait_for(|| MIXES.load(Ordering::Acquire) == 1);
    fixture.collection.work.flush().unwrap();
    let switched = fixture.selected(second);
    let demand = fixture.collection.collector.state.lock().has_demand;
    fixture.finish();
    assert!(entered && mixed && switched);
    assert!(!demand);
}

#[def_test(serial)]
fn absent_source_keeps_demand_and_later_available_resumes_without_polling() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    fixture.collection.request();
    fixture.collection.work.flush().unwrap();
    let waiting = fixture.collection.collector.state.lock().has_demand;
    let ignored = fixture.add("untrusted-other-rng", Response::Ready);
    fixture.collection.work.flush().unwrap();
    let retained = fixture.collection.collector.state.lock().has_demand;
    let accepted = fixture.add("virtio-rng-late", Response::Ready);
    let mixed = wait_for(|| MIXES.load(Ordering::Acquire) == 1);
    fixture.collection.work.flush().unwrap();
    let selected = fixture.selected(accepted);
    let ignored_reads = fixture.providers[ignored]
        .hardware
        .reads
        .load(Ordering::Acquire);
    fixture.finish();
    assert!(waiting && retained && mixed && selected);
    assert_eq!(ignored_reads, 0);
}

#[def_test(serial)]
fn terminal_instances_do_not_restart_and_retired_failure_records_are_pruned() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    let invalid = fixture.add("virtio-rng-invalid", Response::Invalid);
    let failed = fixture.add("virtio-rng-failed", Response::Failed);
    fixture.collection.request();
    fixture.collection.work.flush().unwrap();
    let failures = fixture.collection.collector.state.lock().failed.len();
    for _ in 0..3 {
        fixture
            .collection
            .note_change(fixture.providers[invalid].registration.publication());
        fixture.collection.request();
        fixture.collection.work.flush().unwrap();
    }
    let reads = (
        fixture.providers[invalid]
            .hardware
            .reads
            .load(Ordering::Acquire),
        fixture.providers[failed]
            .hardware
            .reads
            .load(Ordering::Acquire),
    );
    fixture.providers[invalid].unregister();
    fixture.providers[failed].unregister();
    fixture.collection.work.flush().unwrap();
    let pruned = fixture.collection.collector.state.lock().failed.is_empty();
    fixture.add("virtio-rng-invalid", Response::Ready);
    let resumed = wait_for(|| MIXES.load(Ordering::Acquire) == 1);
    fixture.finish();
    assert_eq!(failures, 2);
    assert_eq!(reads, (1, 1));
    assert!(pruned && resumed);
}

#[def_test(serial)]
fn transient_errors_keep_demand_beyond_three_retries() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    for error in [KError::WouldBlock, KError::NoMemory, KError::InvalidInput] {
        let mut fixture = Fixture::new(&REGISTRY);
        let provider = fixture.add("virtio-rng-retry", Response::Retry(error, 4));
        fixture.collection.request();
        let mixed = wait_for(|| MIXES.load(Ordering::Acquire) == 1);
        fixture.collection.work.flush().unwrap();
        let failed = fixture.collection.collector.state.lock().failed.len();
        let reads = fixture.providers[provider]
            .hardware
            .reads
            .load(Ordering::Acquire);
        fixture.finish();
        assert!(mixed);
        assert_eq!(failed, 0);
        assert_eq!(reads, 5);
    }
}

#[def_test(serial)]
fn request_at_worker_return_is_not_lost() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    fixture.add("virtio-rng-tail", Response::Ready);
    fixture.collection.work.flush().unwrap();
    let weak = Arc::downgrade(&fixture.collection);
    *fixture.collection.collector.tail_hook.lock() = Some(Box::new(move || {
        if let Some(collection) = weak.upgrade() {
            collection.request();
        }
    }));
    fixture.collection.request();
    let mixed = wait_for(|| MIXES.load(Ordering::Acquire) == 2);
    fixture.finish();
    assert!(mixed);
}

#[def_test(serial)]
fn blocked_read_coalesces_followup_demand_and_reuses_work() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    let provider = fixture.add("virtio-rng-coalesce", Response::Blocked);
    fixture.collection.request();
    let entered = wait_for(|| {
        fixture.providers[provider]
            .hardware
            .reads
            .load(Ordering::Acquire)
            == 1
    });
    for _ in 0..10 {
        fixture.collection.request();
    }
    *fixture.providers[provider].hardware.response.lock() = Response::Ready;
    fixture.providers[provider].hardware.ready.notify_all();
    let coalesced = wait_for(|| MIXES.load(Ordering::Acquire) == 2);
    fixture.collection.work.flush().unwrap();
    let reads = fixture.providers[provider]
        .hardware
        .reads
        .load(Ordering::Acquire);
    fixture.collection.request();
    let reused = wait_for(|| MIXES.load(Ordering::Acquire) == 3);
    fixture.finish();
    assert!(entered && coalesced && reused);
    assert_eq!(reads, 2);
}

#[def_test(serial)]
fn availability_at_worker_return_resumes_retained_demand() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let mut fixture = Fixture::new(&REGISTRY);
    let hardware = Arc::new(Hardware {
        response: SpinNoIrq::new(Response::Ready),
        reads: AtomicUsize::new(0),
        ready: WaitQueue::new(),
    });
    let slot = Arc::new(SpinNoIrq::new(None));
    *fixture.collection.collector.tail_hook.lock() = Some(Box::new({
        let slot = slot.clone();
        let hardware = hardware.clone();
        move || {
            let registration = REGISTRY
                .attach_legacy(
                    "virtio-rng-tail-event",
                    "test-rng".into(),
                    hardware,
                    Arc::new(Drain(WaitQueue::new())),
                )
                .unwrap();
            *slot.lock() = Some(registration);
        }
    }));
    fixture.collection.request();
    let resumed = wait_for(|| MIXES.load(Ordering::Acquire) == 1);
    fixture.collection.work.flush().unwrap();
    if let Some(registration) = slot.lock().take() {
        fixture.providers.push(Provider {
            registration,
            hardware,
        });
    }
    fixture.finish();
    assert!(resumed);
    assert!(REGISTRY.devices().is_empty());
}
