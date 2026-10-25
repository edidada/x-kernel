// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO demand-driven entropy collection (requires Host trust).

#[cfg(unittest)]
use alloc::boxed::Box;
use alloc::{sync::Arc, vec::Vec};

use kalloc_types::KBox;
use kerrno::{KError, KResult};
use klazy::Lazy;
use kspin::SpinNoPreempt;
use ksync::Mutex;
use kworkng::{HandleCreateError, SubmitError, WorkHandle};
use subsys_hwrng::{HWRNG, HwrngHandle, HwrngRegistry};

const READ_RETRY_DELAY: ktime_types::TimeSpan = ktime_types::TimeSpan::from_millis(100);

static COLLECTION: Lazy<Mutex<Option<Arc<EntropyCollection>>>> = Lazy::new(|| Mutex::new(None));

type CollectorWork = WorkHandle<KBox<dyn Fn() + Send + Sync>>;

struct EntropyCollection {
    collector: Arc<EntropyCollector>,
    work: CollectorWork,
}

struct EntropyCollector {
    registry: &'static HwrngRegistry,
    state: SpinNoPreempt<CollectorState>,
    mix: fn(&[u8]),
    #[cfg(unittest)]
    tail_hook: SpinNoPreempt<Option<Box<dyn FnOnce() + Send>>>,
}

struct CollectorState {
    has_demand: bool,
    selected: Option<HwrngHandle>,
    failed: Vec<HwrngHandle>,
}

impl EntropyCollection {
    /// Prepares a collector in sleepable, reclaim-safe task context with IRQs
    /// and preemption enabled, outside non-sleepable RCU sections. Source,
    /// collector and class-registry locks must not enclose this call. The KERNEL
    /// callback allocation precedes registry subscription and publication.
    /// Nested Arc and worker allocations keep their existing failure behavior.
    fn new(registry: &'static HwrngRegistry, mix: fn(&[u8])) -> KResult<Arc<Self>> {
        if !ktask::is_sleepable_context() {
            return Err(KError::InvalidInput);
        }
        let collector = Arc::new(EntropyCollector {
            registry,
            state: SpinNoPreempt::new(CollectorState {
                has_demand: false,
                selected: None,
                failed: Vec::new(),
            }),
            mix,
            #[cfg(unittest)]
            tail_hook: SpinNoPreempt::new(None),
        });
        let callback = collector.clone();
        let callback: KBox<dyn Fn() + Send + Sync> = KBox::new(move || callback.run())?;
        let work = WorkHandle::new(callback).map_err(|error| match error {
            HandleCreateError::InvalidContext => KError::InvalidInput,
            HandleCreateError::ServicesUnavailable => KError::OperationNotSupported,
        })?;
        let collection = Arc::new(Self { collector, work });
        for subscribe in [
            HwrngRegistry::subscribe_available,
            HwrngRegistry::subscribe_removed,
        ] {
            let weak = Arc::downgrade(&collection);
            subscribe(
                registry,
                Arc::new(move |device| {
                    if let Some(collection) = weak.upgrade() {
                        collection.note_change(device);
                    }
                }),
            );
        }
        collection.kick();
        Ok(collection)
    }

    fn kick(&self) {
        match self.work.queue_default(None) {
            Ok(_) | Err(SubmitError::AlreadyQueued) => {}
            Err(error) => log::warn!("entropy: failed to queue VirtIO entropy work: {error:?}"),
        }
    }

    fn note_change(&self, _device: HwrngHandle) {
        self.kick();
    }

    fn request(&self) {
        self.collector.state.lock().has_demand = true;
        self.kick();
    }
}

impl EntropyCollector {
    fn refresh(&self) {
        let devices: Vec<_> = self
            .registry
            .devices()
            .into_iter()
            .filter(|device| is_virtio_rng(device.driver_name().unwrap_or("")))
            .collect();
        let (selected, mut failed) = {
            let state = self.state.lock();
            (state.selected.clone(), state.failed.clone())
        };
        failed.retain(|old| devices.iter().any(|device| old.same_publication(device)));
        let eligible =
            |device: &HwrngHandle| !failed.iter().any(|old| old.same_publication(device));
        let selected = selected
            .filter(|old| {
                devices.iter().any(|device| old.same_publication(device)) && eligible(old)
            })
            .or_else(|| devices.into_iter().find(eligible));
        let retired = {
            let mut state = self.state.lock();
            (
                core::mem::replace(&mut state.selected, selected),
                core::mem::replace(&mut state.failed, failed),
            )
        };
        drop(retired);
    }

    fn retain_demand(&self, device: &HwrngHandle, is_terminal: bool) {
        let mut failed = self.state.lock().failed.clone();
        if is_terminal
            && device.is_available()
            && !failed.iter().any(|old| old.same_publication(device))
        {
            failed.push(device.clone());
        }
        let retired = {
            let mut state = self.state.lock();
            state.has_demand = true;
            core::mem::replace(&mut state.failed, failed)
        };
        drop(retired);
    }

    fn run(&self) {
        self.collect();
        #[cfg(unittest)]
        {
            let hook = self.tail_hook.lock().take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    fn collect(&self) {
        loop {
            self.refresh();
            let device = {
                let mut state = self.state.lock();
                if !state.has_demand {
                    return;
                }
                let Some(device) = state.selected.clone() else {
                    return;
                };
                state.has_demand = false;
                device
            };
            let mut bytes = [0; super::HW_RESEED_BYTES];
            let mut has_reported_error = false;
            loop {
                match device.read(&mut bytes) {
                    Ok(len) if len > 0 && len <= bytes.len() => {
                        (self.mix)(&bytes[..len]);
                        return;
                    }
                    Err(error) => match device.is_failed() {
                        Err(_) => {
                            self.retain_demand(&device, false);
                            break;
                        }
                        Ok(true) => {
                            log::warn!("entropy: VirtIO entropy device failed: {error:?}");
                            self.retain_demand(&device, true);
                            break;
                        }
                        Ok(false) => {
                            if !has_reported_error {
                                log::warn!("entropy: retrying VirtIO entropy read: {error:?}");
                                has_reported_error = true;
                            }
                            ktask::sleep(READ_RETRY_DELAY);
                        }
                    },
                    Ok(len) => {
                        log::warn!(
                            "entropy: disabling VirtIO entropy instance after invalid read \
                             length: {len}"
                        );
                        self.retain_demand(&device, true);
                        break;
                    }
                }
            }
        }
    }
}

pub(crate) fn is_present() -> bool {
    let collection = COLLECTION.lock().clone();
    let selected =
        collection.and_then(|collection| collection.collector.state.lock().selected.clone());
    selected.is_some_and(|device| device.is_available())
}

pub(crate) fn is_disabled() -> bool {
    COLLECTION.lock().as_ref().is_some_and(|collection| {
        let state = collection.collector.state.lock();
        state.selected.is_none() && !state.failed.is_empty()
    })
}

pub(crate) fn register_sources() {
    if COLLECTION.lock().is_some() {
        return;
    }
    let prepared = match EntropyCollection::new(&HWRNG, super::mix_virtio_entropy) {
        Ok(prepared) => prepared,
        Err(error) => {
            log::warn!("entropy: failed to prepare VirtIO entropy work: {error:?}");
            return;
        }
    };
    let mut collection = COLLECTION.lock();
    if collection.is_none() {
        *collection = Some(prepared);
        return;
    }
    drop(collection);
    drop(prepared);
}

pub(crate) fn request_entropy() {
    let collection = COLLECTION.lock().clone();
    if let Some(collection) = collection {
        collection.request();
    }
}

fn is_virtio_rng(driver_name: &str) -> bool {
    driver_name.contains("virtio-rng")
}

unittest::path_mod_test!(real_device_tests, "../ut-tests/virtio_real_device_tests.rs");
unittest::path_mod_test!(tests, "../ut-tests/virtio.rs");
