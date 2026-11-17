// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native x86 source ownership; KIRQ owns action publication and retirement.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use kplat::nm_irq::{NativeNmiSource as Source, NativeNmiSourceIf, NmiCb, NmiMode};
#[cfg(feature = "nmi-pmu")]
use x86_pmu::Counter;

mod lifecycle;
#[cfg(feature = "nmi-peer-timer")]
mod peer;

pub(super) fn descriptor(source: Source) -> kirq::IrqDesc {
    irq_driver::x86::nmi::descriptor(source)
}

struct LocalSource {
    #[cfg(feature = "nmi-pmu")]
    counter: klazy::Once<Option<Counter>>,
    callback: AtomicPtr<()>,
    preload: AtomicU64,
    is_armed: AtomicBool,
    is_reserved: AtomicBool,
    periodic_pending: AtomicBool,
    periodic_enabled: AtomicBool,
    notification_enabled: AtomicBool,
    notification_pending: AtomicBool,
}
impl LocalSource {
    const fn new() -> Self {
        Self {
            #[cfg(feature = "nmi-pmu")]
            counter: klazy::Once::new(),
            callback: AtomicPtr::new(core::ptr::null_mut()),
            preload: AtomicU64::new(0),
            is_armed: AtomicBool::new(false),
            is_reserved: AtomicBool::new(false),
            periodic_pending: AtomicBool::new(false),
            periodic_enabled: AtomicBool::new(false),
            notification_enabled: AtomicBool::new(false),
            notification_pending: AtomicBool::new(false),
        }
    }

    #[cfg(feature = "nmi-pmu")]
    fn hardware(&self) -> Option<Counter> {
        self.counter.get().copied().flatten()
    }

    fn callback(&self) {
        let pointer = self.callback.load(Ordering::Acquire);
        if !pointer.is_null() {
            // SAFETY: only source arming writes this slot, from a static fn().
            // Local KIRQ publication is revoked before shutdown clears it;
            // Acquire observes the callback before is_armed/enable publication.
            let callback: NmiCb = unsafe { core::mem::transmute(pointer) };
            callback();
        }
    }

    fn quiesce(&self) {
        if self.is_armed.swap(false, Ordering::AcqRel) {
            ARMED_CPUS.fetch_and(!(1usize << kplat::cpu::id().as_usize()), Ordering::Release);
            #[cfg(feature = "nmi-pmu")]
            if let Some(counter) = self.hardware() {
                // SAFETY: arming exclusively reserved this CPU's PMU. Terminal
                // quiescence retains registers/storage and is safe from NMI.
                unsafe { counter.stop() };
                // SAFETY: reserved LVTPC on the same pinned initialized CPU.
                unsafe { x86_apic::set_local_performance_nmi_enabled(false) };
            }
        }
    }
}
// SAFETY: empty Once and reset atomics own no heap/pointers; each copied CPU
// template initializes its independent immutable capability snapshot at boot.
unsafe impl kcpu_slot::StaticSlotValue for LocalSource {}

kcpu_slot::cpu_slot! {
    static LOCAL: crate::peripherals::nmi::LocalSource = crate::peripherals::nmi::LocalSource::new();
}
static ARMED_CPUS: AtomicUsize = AtomicUsize::new(0);
static REGISTERED: AtomicBool = AtomicBool::new(false);
static REGISTERED_SOURCES: AtomicUsize = AtomicUsize::new(0);
const _: () = assert!(
    kbuild_config::NR_CPUS <= usize::BITS as usize,
    "native NMI membership mask cannot represent configured CPUs"
);

#[kiface::provide]
impl kplat::nm_irq::NmiMechanismIf {
    fn early_init() -> bool {
        true
    }

    fn late_init() -> bool {
        #[cfg(feature = "nmi-pmu")]
        LOCAL.current_shared().counter.call_once(Counter::probe);
        true
    }

    fn mode() -> NmiMode {
        NmiMode::Hardware
    }

    fn info() -> kplat::nm_irq::NmiSourceInfo {
        kplat::nm_irq::NmiSourceInfo {
            name: "x86 native NMI (vector 2)",
        }
    }
}

#[kiface::provide]
impl NativeNmiSourceIf {
    fn claim_notification() -> bool {
        let local = LOCAL.current_shared();
        local.notification_enabled.load(Ordering::Acquire)
            && local.notification_pending.swap(false, Ordering::AcqRel)
    }

    fn claim_periodic() -> bool {
        let local = LOCAL.current_shared();
        if !local.is_armed.load(Ordering::Acquire)
            || !local.periodic_enabled.load(Ordering::Acquire)
        {
            return false;
        }
        #[cfg(feature = "nmi-peer-timer")]
        return local.periodic_pending.swap(false, Ordering::AcqRel);
        #[cfg(feature = "nmi-pmu")]
        {
            let Some(counter) = local.hardware() else {
                return false;
            };
            // SAFETY: armed publication proves local reservation and initialization.
            if !unsafe { counter.has_overflow() } {
                return false;
            }
            // SAFETY: same owned counter; stop before KIRQ invokes consumer code.
            unsafe { counter.stop() };
            true
        }
    }

    fn prepare(source: Source) -> bool {
        let local = LOCAL.current_shared();
        match source {
            Source::Periodic => {
                !local.periodic_enabled.load(Ordering::Acquire)
                    && !local.is_armed.load(Ordering::Acquire)
                    && local.is_reserved.load(Ordering::Acquire)
            }
            Source::Notification => !local.notification_enabled.load(Ordering::Acquire),
        }
    }

    fn teardown(source: Source) -> bool {
        let local = LOCAL.current_shared();
        match source {
            Source::Periodic => {
                !local.periodic_enabled.load(Ordering::Acquire)
                    && !local.is_armed.load(Ordering::Acquire)
            }
            Source::Notification => {
                if local.notification_enabled.load(Ordering::Acquire) {
                    return false;
                }
                local.notification_pending.store(false, Ordering::Release);
                true
            }
        }
    }

    fn set_enabled(source: Source, enabled: bool) {
        let local = LOCAL.current_shared();
        match source {
            Source::Periodic => {
                local.periodic_enabled.store(enabled, Ordering::Release);
                #[cfg(feature = "nmi-pmu")]
                if local.is_reserved.load(Ordering::Acquire) {
                    // SAFETY: local PMU capability validated LVTPC, and source
                    // control retains CPU/IRQ exclusion. This does not start it.
                    unsafe { x86_apic::set_local_performance_nmi_enabled(enabled) };
                }
            }
            Source::Notification => local.notification_enabled.store(enabled, Ordering::Release),
        }
    }

    fn shutdown_sources() -> kerrno::KResult<()> {
        lifecycle::shutdown()
    }

    fn restore_sources() -> kerrno::KResult<()> {
        lifecycle::restore()
    }

    fn complete_periodic() {
        #[cfg(feature = "nmi-pmu")]
        {
            let local = LOCAL.current_shared();
            if local.is_armed.load(Ordering::Acquire)
                && local.periodic_enabled.load(Ordering::Acquire)
                && let Some(counter) = local.hardware()
            {
                // SAFETY: claim stopped the reserved producer. The callback may
                // quiesce it; armed/enable admission above prevents resurrection.
                unsafe { counter.reload(local.preload.load(Ordering::Relaxed)) };
                // SAFETY: status is cleared and payload remains published; late
                // LVTPC unmask avoids acknowledging an uncleared overflow.
                unsafe { x86_apic::set_local_performance_nmi_enabled(true) };
                // SAFETY: source is still exclusively reserved and enabled.
                unsafe { counter.start() };
            }
        }
    }
}

fn register(startup: bool) -> bool {
    for (index, source) in [Source::Periodic, Source::Notification]
        .into_iter()
        .enumerate()
    {
        if REGISTERED_SOURCES.load(Ordering::Acquire) & (1 << index) != 0 {
            continue;
        }
        let desc = descriptor(source);
        if kirq::try_map(desc).is_err() {
            return false;
        }
        let callback = Arc::new(|_| {
            LOCAL.current_shared().callback();
            kirq::IrqEvent::HANDLED
        });
        let registered = if startup {
            kirq::register_nmi_startup(desc, callback)
        } else {
            kirq::register_nmi(desc, callback)
        };
        if !registered {
            return false;
        }
        REGISTERED_SOURCES.fetch_or(1 << index, Ordering::Release);
    }
    REGISTERED.store(true, Ordering::Release);
    true
}

#[cfg(feature = "nmi-pmu")]
#[kiface::provide]
impl kplat::perf::PmuInterruptIf {
    fn register_overflow_irq() -> bool {
        register(true)
    }

    fn enable_irq() {}

    // Each local source transaction enables its own delivery.
    fn shutdown_overflow_irq() -> kerrno::KResult<()> {
        NativeNmiSourceIf::shutdown_sources()
    }

    fn restore_overflow_irq() -> kerrno::KResult<()> {
        NativeNmiSourceIf::restore_sources()
    }
}

#[kiface::provide]
impl kplat::nm_irq::PeriodicNmiSourceIf {
    fn register_source() -> bool {
        register(true)
    }

    fn enable_periodic_nmi(period_ns: u64, handler: NmiCb) -> bool {
        arm(period_ns, handler)
    }

    fn quiesce_periodic_nmi() {
        LOCAL.current_shared().quiesce();
    }

    fn shutdown_periodic_nmi() -> bool {
        shutdown_local()
    }

    fn request_peer_samples() -> bool {
        notify_peers()
    }
}

fn arm(period_ns: u64, callback: NmiCb) -> bool {
    if kirq::context::is_in_nmi() {
        return false;
    }
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = LOCAL.current_shared();
    if !REGISTERED.load(Ordering::Acquire)
        || !lifecycle::can_start()
        || local.is_armed.load(Ordering::Acquire)
    {
        return false;
    }
    #[cfg(feature = "nmi-peer-timer")]
    if kcpu_id_map::nr_cpus() < 2 || !cfg!(feature = "smp") || period_ns == 0 {
        return false;
    }
    #[cfg(feature = "nmi-pmu")]
    let (counter, preload) = {
        let Some(counter) = local.hardware() else {
            log::warn!(
                "x86 periodic NMI: supported PMU unavailable on cpu {}",
                kplat::cpu::id().as_usize()
            );
            return false;
        };
        // SAFETY: CPUID validated registers and the pinned CPU has no owner.
        if !unsafe { counter.is_vacant() } {
            return false;
        }
        let Some(preload) = counter.preload(period_ns, khal::time::frequency().as_hz()) else {
            return false;
        };
        (counter, preload)
    };
    local.is_reserved.store(true, Ordering::Release);
    #[cfg(feature = "nmi-pmu")]
    // SAFETY: exclusive vacancy was established; mask before preparing the Line.
    unsafe {
        x86_apic::set_local_performance_nmi_enabled(false)
    };
    for source in [Source::Periodic, Source::Notification] {
        if kirq::prepare_percpu_nmi(descriptor(source)).is_err() {
            let _ = shutdown_local();
            return false;
        }
    }
    local.notification_pending.store(false, Ordering::Release);
    local.periodic_pending.store(false, Ordering::Release);
    local.callback.store(callback as *mut (), Ordering::Release);
    #[cfg(feature = "nmi-pmu")]
    local.preload.store(preload, Ordering::Relaxed);
    #[cfg(feature = "nmi-peer-timer")]
    local.preload.store(period_ns, Ordering::Relaxed);
    // SAFETY: vacancy and KIRQ preparation retain exclusive ownership of the
    // stopped counter and disabled route; payload is published before delivery.
    #[cfg(feature = "nmi-pmu")]
    unsafe {
        counter.initialize(preload)
    };
    local.is_armed.store(true, Ordering::Release);
    for source in [Source::Periodic, Source::Notification] {
        if kirq::try_enable_irq(descriptor(source)).is_err() {
            let _ = shutdown_local();
            return false;
        }
    }
    ARMED_CPUS.fetch_or(1usize << kplat::cpu::id().as_usize(), Ordering::Release);
    // SAFETY: callback and both local actions are published before counting.
    #[cfg(feature = "nmi-pmu")]
    unsafe {
        counter.start()
    };
    #[cfg(feature = "nmi-peer-timer")]
    if !peer::start() {
        let _ = shutdown_local();
        return false;
    }
    true
}

fn shutdown_local() -> bool {
    if kirq::context::is_in_nmi() {
        return false;
    }
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = LOCAL.current_shared();
    let was_owned =
        local.is_armed.load(Ordering::Acquire) || !local.callback.load(Ordering::Acquire).is_null();
    local.quiesce();
    for source in [Source::Periodic, Source::Notification] {
        let desc = descriptor(source);
        if kirq::try_disable_irq_nosync(desc).is_err() {
            return false;
        }
        // SAFETY: local producers are stopped, ordinary IRQs excluded, and no
        // outstanding NMI invocation can overlap this task/IPI teardown scope.
        if unsafe { kirq::teardown_percpu_nmi(desc) }.is_err() {
            return false;
        }
    }
    #[cfg(feature = "nmi-pmu")]
    if was_owned && let Some(counter) = local.hardware() {
        // SAFETY: local actions are revoked and the reserved producer is stopped.
        unsafe { counter.release() };
    }
    let _ = was_owned;
    local.is_reserved.store(false, Ordering::Release);
    local.periodic_pending.store(false, Ordering::Release);
    local
        .callback
        .store(core::ptr::null_mut(), Ordering::Release);
    ARMED_CPUS.fetch_and(!(1usize << kplat::cpu::id().as_usize()), Ordering::Release);
    true
}

fn notify_peers() -> bool {
    let this = kplat::cpu::id();
    let mask = ARMED_CPUS.load(Ordering::Acquire);
    let mut complete = true;
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
        if cpu == this || mask & (1usize << cpu.as_usize()) == 0 {
            return;
        }
        let Some(area) = kcpu_slot::area(cpu) else {
            complete = false;
            return;
        };
        LOCAL
            .at(&area)
            .notification_pending
            .store(true, Ordering::Release);
        let Some(raw) = kcpu_id_map::raw_cpu_id(cpu) else {
            complete = false;
            return;
        };
        // SAFETY: armed membership includes only CPUs with initialized native
        // NMI entry and notification action. The caller is pinned IRQ/NMI context.
        if unsafe { x86_apic::try_send_nmi_raw(raw) }.is_err() {
            complete = false;
        }
    });
    complete
}

unittest::path_mod_test!(tests, "../../ut-tests/nmi.rs");
