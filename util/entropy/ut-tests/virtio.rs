// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktask::WaitQueue;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_is_virtio_rng_name_matching() {
    assert!(is_virtio_rng("virtio-rng", "unknown"));
    assert!(is_virtio_rng("hwrng0", "virtio-rng"));
    assert!(is_virtio_rng("rng", "platform-virtio-rng-pci"));
    assert!(!is_virtio_rng("ttyS0", "uart"));
    assert!(!is_virtio_rng("virtio-blk", "virtio-blk"));
}

use core::sync::atomic::{AtomicUsize, Ordering};

use subsys_hwrng::{HwrngClass, HwrngDevice, KError, KResult};

#[derive(Default)]
struct HardwareState {
    is_ready: bool,
    fault: Option<KError>,
    is_failed: bool,
}
struct Hardware {
    state: Arc<SpinNoIrq<HardwareState>>,
    reads: AtomicUsize,
    ready: WaitQueue,
    entered: WaitQueue,
}
impl Hardware {
    fn wait_reads(&self, count: usize) {
        self.entered
            .wait_until(|| self.reads.load(Ordering::Acquire) >= count);
    }

    fn complete(&self) {
        self.state.lock().is_ready = true;
        self.ready.notify_all(false);
    }

    fn fail(&self, error: KError) {
        self.state.lock().fault = Some(error);
        self.ready.notify_all(false);
    }
}
impl HwrngDevice for Hardware {
    fn is_failed(&self) -> bool {
        self.state.lock().is_failed
    }

    fn read(&self, output: &mut [u8]) -> KResult<usize> {
        self.reads.fetch_add(1, Ordering::Release);
        self.entered.notify_all(false);
        self.ready.wait_until(|| {
            let state = self.state.lock();
            state.is_ready || state.fault.is_some()
        });
        let mut state = self.state.lock();
        if let Some(error) = state.fault.take() {
            return Err(error);
        }
        state.is_ready = false;
        output.fill(0x5a);
        Ok(output.len())
    }
}

#[def_test(serial)]
fn withdrawn_source_stops_collector_without_calling_hardware() {
    static CLASS: HwrngClass = HwrngClass::new();
    let hardware = Arc::new(Hardware {
        state: Arc::new(SpinNoIrq::new(HardwareState::default())),
        reads: AtomicUsize::new(0),
        ready: WaitQueue::new(),
        entered: WaitQueue::new(),
    });
    let registration = CLASS
        .attach_legacy(
            "entropy-collector-test",
            alloc::string::String::from("withdrawn"),
            hardware.clone(),
            "virtio-rng-test",
        )
        .unwrap();
    let source = VirtioEntropySource::new(registration.publication()).unwrap();
    drop(registration);
    source.request();
    source.work.flush().unwrap();
    assert!(source.collector.state.lock().is_failed);
    assert_eq!(hardware.reads.load(Ordering::Acquire), 0);
    source.work.shutdown().unwrap();
}
#[def_test(serial)]
fn collector_retries_without_new_demand_and_stops_only_on_device_failure() -> unittest::TestResult {
    for index in 0..3u32 {
        let hardware = Arc::new(Hardware {
            state: Arc::new(SpinNoIrq::new(HardwareState::default())),
            reads: AtomicUsize::new(0),
            ready: WaitQueue::new(),
            entered: WaitQueue::new(),
        });
        static CLASS: HwrngClass = HwrngClass::new();
        let registration = CLASS
            .attach_legacy(
                "entropy-collector-test",
                alloc::string::String::from("ram-disk"),
                hardware.clone(),
                "virtio-rng-test",
            )
            .unwrap();
        let class = registration.publication();
        let source = VirtioEntropySource::new(class.clone()).unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), 0);
        source.request();
        hardware.wait_reads(1);
        // Requests while the first read is blocked coalesce into one follow-up.
        for _ in 0..10 {
            source.request();
        }
        hardware.complete();
        hardware.wait_reads(2);
        hardware.complete();
        source.work.flush().unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), 2);
        assert!(!source.collector.state.lock().has_demand);

        // The same prepared Work accepts a fresh demand after returning idle.
        source.request();
        hardware.wait_reads(3);
        let error = match index {
            0 => KError::WouldBlock,
            1 => KError::NoMemory,
            _ => KError::InvalidInput,
        };
        // Retry without a new demand, beyond the old three-error cutoff.
        for attempt in 0..4 {
            hardware.fail(error);
            hardware.wait_reads(4 + attempt);
            assert!(!source.collector.state.lock().is_failed);
        }
        hardware.complete();
        source.work.flush().unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), 7);

        hardware.state.lock().is_failed = true;
        hardware.fail(KError::Io);
        source.request();
        source.work.flush().unwrap();
        assert!(source.collector.state.lock().is_failed);
        let reads = hardware.reads.load(Ordering::Acquire);
        source.request();
        source.work.flush().unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), reads);
        source.work.shutdown().unwrap();
        drop(class);
        drop(registration);
    }
    unittest::TestResult::Ok
}
