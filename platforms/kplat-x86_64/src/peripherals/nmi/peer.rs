// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit SMP-only periodic source when the machine exposes no PMU.

use ktime_types::TimeSpan;

use super::*;

struct PeerTimer;
impl ktask::TimerHandler for PeerTimer {
    fn on_expiration(&self, timer: ktask::TimerControl<'_>, _: ktask::Expiration) {
        let local = LOCAL.current_shared();
        if !local.is_armed.load(Ordering::Acquire) {
            return;
        }
        let mask = ARMED_CPUS.load(Ordering::Acquire);
        let this = kplat::cpu::id().as_usize();
        // A ring gives each CPU one observer. Never sample ourselves using our
        // own maskable timer; a masked CPU must be reached by another CPU's NMI.
        for step in 1..kbuild_config::NR_CPUS {
            let cpu = kcpu_id_map::LogicalCpuId::new((this + step) % kbuild_config::NR_CPUS);
            if mask & (1usize << cpu.as_usize()) == 0 {
                continue;
            }
            if let (Some(area), Some(raw)) = (kcpu_slot::area(cpu), kcpu_id_map::raw_cpu_id(cpu)) {
                LOCAL
                    .at(&area)
                    .periodic_pending
                    .store(true, Ordering::Release);
                // SAFETY: armed targets have initialized IST/NMI and published
                // local actions; this timer retains CPU/ordinary IRQ exclusion.
                let _ = unsafe { x86_apic::try_send_nmi_raw(raw) };
            }
            break;
        }
        let period = TimeSpan::from_nanos(local.preload.load(Ordering::Relaxed));
        let _ = timer.rearm(khal::time::monotonic_time() + period);
    }
}

// This permanent one-shot owner supports restart without freeing timer storage
// from the NMI/IPI shutdown callback.
struct TimerOwner {
    timer: klazy::Once<ktask::Timer<PeerTimer>>,
}
impl TimerOwner {
    const fn new() -> Self {
        Self {
            timer: klazy::Once::new(),
        }
    }
}
// SAFETY: an empty Once owns no allocation or pointers and each CPU initializes
// its independent permanent timer after scheduler startup.
unsafe impl kcpu_slot::StaticSlotValue for TimerOwner {}
kcpu_slot::cpu_slot! {
    static TIMER: crate::peripherals::nmi::peer::TimerOwner = crate::peripherals::nmi::peer::TimerOwner::new();
}

pub(super) fn start() -> bool {
    let timer = TIMER
        .current_shared()
        .timer
        .call_once(|| ktask::Timer::new(PeerTimer));
    let period = TimeSpan::from_nanos(LOCAL.current_shared().preload.load(Ordering::Relaxed));
    // SAFETY: watchdog arming follows local scheduler/timer preparation. The
    // permanent owner retains storage, and the callback only publishes/sends
    // one NMI then rearms without sleeping, allocating or enabling IRQs.
    unsafe { timer.start(khal::time::monotonic_time() + period) }.is_ok()
}
