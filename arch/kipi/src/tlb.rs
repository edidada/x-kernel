// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TLB shootdown via IPI.
//!
//! When one CPU modifies page tables, other CPUs may hold stale TLB entries.
//! This module provides two IPI-based shootdown paths:
//!
//! 1. **Per-address-space flush** (`flush_process_mask`): the initiator sends
//!    IPIs only to CPUs recorded in the target user address space's mm-owned
//!    residency mask. Used for user page table modifications whose visibility
//!    is scoped to a single address space.
//!
//! 2. **All-CPU flush** (`flush_all_cpus`): the initiator targets every present
//!    CPU after all CPU interrupt services are ready. Shared kernel mappings
//!    stay frozen during AP startup; before it, only the boot CPU needs a flush.
//!
//! Each initiator CPU owns one zero-allocation request slot. A request is
//! identified by the pair `(initiator_cpu, request_seq)`. Remote CPUs acknowledge
//! the highest sequence they have processed for each initiator, so concurrent
//! shootdowns from different CPUs cannot overwrite each other's completion
//! state.
//!
//! Residency maintenance is owned by the scheduler/context-switch path rather
//! than the flush path itself. Shootdown consumes a conservative residency
//! snapshot; it does not rebuild or reset residency after full or partial
//! flushes.
//!
//! Implements the [`page_table::TlbFlushIf`] interface defined in the
//! `page_table` crate, breaking the circular dependency between `page_table` and
//! `kipi`.
//!
//! Runtime calls require task context with IRQs enabled. Hardirq, softirq and
//! NMI code may handle inbound requests but must not initiate invalidation.
//! Bootstrap completion is local before remote services are ready. The transport
//! owns retries and waits for every required acknowledgement; no incomplete
//! completion result reaches callers.
//!
//! Progress requires targets to re-enable IRQs or poll incoming shootdowns
//! independently of the initiating CPU. An IRQ-off target must not wait for a
//! task, generic IPI callback or lock owner that can be stalled by this request.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kbuild_config::IPI_IRQ;
use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, for_each_present_logical_cpu};
use kcpu_slot::cpu_slot_cell;
use khal::percpu::this_cpu_id;
use kirq::TargetCpu;
use kspin::NoPreempt;
use ktime_types::TimeSpan;
use memaddr::VirtAddr;
use page_table::TlbFlushIf;

// Shared kernel mappings are frozen throughout AP startup. Remote flushes
// become necessary only after every CPU can acknowledge them.
static ALL_CPUS_STARTED: AtomicBool = AtomicBool::new(false);

/// Monotonic sequence allocator per initiator CPU.
static REQUEST_SLOTS: [ShootdownRequestSlot; kbuild_config::NR_CPUS] =
    [const { ShootdownRequestSlot::new() }; kbuild_config::NR_CPUS];

#[cfg(unittest)]
static BEFORE_ACK_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Per-target fast path gate for `handle_shootdown()`.
///
/// Each initiator increments the epoch for every target CPU before sending a
/// TLB IPI. The target CPU snapshots its local epoch at IPI entry and skips the
/// O(NR_CPUS) slot scan when no epoch change has occurred since the last TLB
/// scan on that CPU.
///
/// This epoch is a scan hint, not a completion acknowledgement: completion is
/// tracked by each request slot's per-target sequence. Local scalar accesses
/// do not serialize the whole scan against a nested IPI. If a nested scan
/// stores a newer epoch before the outer scan stores its entry snapshot, the
/// older hint forces another scan rather than absorbing the newer request.
static PENDING_EPOCH_BY_CPU: [AtomicU64; kbuild_config::NR_CPUS] =
    [const { AtomicU64::new(0) }; kbuild_config::NR_CPUS];

cpu_slot_cell! {
    static LAST_HANDLED_PENDING_EPOCH: u64 = 0;
}

const SHOOTDOWN_WARN: TimeSpan = TimeSpan::from_secs(1);

/// Retry and diagnostic policy for a target that has not acknowledged the request.
///
/// Re-delivery can recover a delayed/lost notification, including emulator
/// wakeup races. Completion always requires the target's exact request ack:
/// - `SHOOTDOWN_RESEND_INTERVAL`: re-deliver the IPI periodically to recover
///   from a transient delivery loss.
/// - `SHOOTDOWN_ESCALATE_TIMEOUT`: report sustained non-ack periodically; keep waiting
///   because an incomplete flush cannot be returned to page-table callers.
const SHOOTDOWN_RESEND_INTERVAL: TimeSpan = TimeSpan::from_secs(1);
const SHOOTDOWN_ESCALATE_TIMEOUT: TimeSpan = TimeSpan::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestSeq(u64);

impl RequestSeq {
    const INITIAL: Self = Self(0);

    const fn get(self) -> u64 {
        self.0
    }

    const fn next_after(raw: u64) -> Self {
        if raw == u64::MAX {
            Self(1)
        } else {
            Self(raw + 1)
        }
    }
}

/// A request snapshot obtained by acquiring `published_seq`.
///
/// The constructor performs the acquire load on `published_seq`, so the
/// remaining request fields may be read with relaxed ordering through this
/// snapshot while still observing the state published before the matching
/// release store in `ShootdownRequestSlot::publish()`.
struct PublishedShootdownRequest<'a> {
    slot: &'a ShootdownRequestSlot,
    seq: RequestSeq,
}

impl<'a> PublishedShootdownRequest<'a> {
    fn seq(&self) -> RequestSeq {
        self.seq
    }

    fn targets_cpu(&self, cpu: usize) -> bool {
        self.slot.targeted_cpus[cpu].load(Ordering::Relaxed)
    }

    fn flush_vaddr(&self) -> Option<VirtAddr> {
        if self.slot.is_flush_all.load(Ordering::Relaxed) {
            None
        } else {
            Some(VirtAddr::from(
                self.slot.published_vaddr.load(Ordering::Relaxed),
            ))
        }
    }
}

struct ShootdownRequestSlot {
    next_seq: AtomicU64,
    published_seq: AtomicU64,
    published_vaddr: AtomicUsize,
    is_flush_all: AtomicBool,
    is_active: AtomicBool,
    targeted_cpus: [AtomicBool; kbuild_config::NR_CPUS],
    acked_seq_by_cpu: [AtomicU64; kbuild_config::NR_CPUS],
}

impl ShootdownRequestSlot {
    const fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(RequestSeq::INITIAL.get()),
            published_seq: AtomicU64::new(RequestSeq::INITIAL.get()),
            published_vaddr: AtomicUsize::new(0),
            is_flush_all: AtomicBool::new(false),
            is_active: AtomicBool::new(false),
            targeted_cpus: [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS],
            acked_seq_by_cpu: [const { AtomicU64::new(RequestSeq::INITIAL.get()) };
                kbuild_config::NR_CPUS],
        }
    }

    #[cfg(unittest)]
    fn reset_for_test(&self) {
        self.next_seq
            .store(RequestSeq::INITIAL.get(), Ordering::Relaxed);
        self.published_seq
            .store(RequestSeq::INITIAL.get(), Ordering::Relaxed);
        self.published_vaddr.store(0, Ordering::Relaxed);
        self.is_flush_all.store(false, Ordering::Relaxed);
        self.is_active.store(false, Ordering::Relaxed);
        for is_targeted in &self.targeted_cpus {
            is_targeted.store(false, Ordering::Relaxed);
        }
        for acked_seq in &self.acked_seq_by_cpu {
            acked_seq.store(RequestSeq::INITIAL.get(), Ordering::Relaxed);
        }
    }

    fn activate(&self) {
        assert!(
            self.is_active
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "TLB shootdown re-entered on one CPU"
        );
    }

    fn deactivate(&self) {
        let was_active = self.is_active.swap(false, Ordering::AcqRel);
        debug_assert!(
            was_active,
            "deactivating an inactive shootdown request slot"
        );
    }

    fn allocate_seq(&self) -> RequestSeq {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: fetch_update cannot return Err \
                      because its closure unconditionally returns Some"
        )]
        let previous = self
            .next_seq
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(RequestSeq::next_after(current).get())
            })
            .expect("fetch_update closure always returns Some");
        let seq = RequestSeq::next_after(previous);
        debug_assert_ne!(seq, RequestSeq::INITIAL);
        seq
    }

    fn clear_targets(&self) {
        for is_targeted in &self.targeted_cpus {
            is_targeted.store(false, Ordering::Relaxed);
        }
    }

    fn mark_target(&self, target_cpu: LogicalCpuId) {
        self.targeted_cpus[target_cpu.as_usize()].store(true, Ordering::Relaxed);
    }

    fn publish(&self, request_seq: RequestSeq, vaddr: Option<VirtAddr>) {
        match vaddr {
            Some(va) => {
                self.is_flush_all.store(false, Ordering::Relaxed);
                self.published_vaddr.store(va.as_usize(), Ordering::Relaxed);
            }
            None => {
                self.is_flush_all.store(true, Ordering::Relaxed);
            }
        }
        self.published_seq
            .store(request_seq.get(), Ordering::Release);
    }

    fn load_published_request(&self) -> Option<PublishedShootdownRequest<'_>> {
        let seq = RequestSeq(self.published_seq.load(Ordering::Acquire));
        if seq == RequestSeq::INITIAL {
            return None;
        }
        Some(PublishedShootdownRequest { slot: self, seq })
    }

    fn is_acked_by(&self, cpu: usize, request_seq: RequestSeq) -> bool {
        self.acked_seq_by_cpu[cpu].load(Ordering::Acquire) == request_seq.get()
    }

    fn ack(&self, cpu: usize, request_seq: RequestSeq) {
        self.acked_seq_by_cpu[cpu].store(request_seq.get(), Ordering::Release);
    }

    fn targeted_snapshot(&self) -> [bool; kbuild_config::NR_CPUS] {
        core::array::from_fn(|cpu| self.targeted_cpus[cpu].load(Ordering::Relaxed))
    }

    fn acked_snapshot(&self) -> [u64; kbuild_config::NR_CPUS] {
        core::array::from_fn(|cpu| self.acked_seq_by_cpu[cpu].load(Ordering::Relaxed))
    }
}

/// Active borrow of the current CPU's shootdown request slot.
///
/// The guard disables preemption while leaving IRQs enabled. Another task
/// cannot reuse this CPU's slot before completion, and interrupt callbacks
/// cannot initiate shootdown. Incoming IPIs can acknowledge concurrent requests.
///
/// Important: this guard must stay scoped to the request-slot publish/send/wait
/// window only. It must be dropped before any path that can block or sleep.
struct ActiveShootdownSlot<'a> {
    _guard: NoPreempt,
    slot: &'a ShootdownRequestSlot,
    initiator: usize,
}

impl<'a> ActiveShootdownSlot<'a> {
    fn acquire_current() -> Self {
        assert!(
            karch::local_irq_enabled(),
            "runtime TLB shootdown requires enabled IRQs"
        );
        assert!(
            !kirq::context::is_in_hardirq()
                && !kirq::context::is_serving_softirq()
                && !kirq::context::is_in_nmi(),
            "TLB shootdown must not be initiated from hardirq, softirq or NMI context"
        );
        let guard = NoPreempt::new();
        let initiator = this_cpu_id().as_usize();
        let slot = &REQUEST_SLOTS[initiator];
        slot.activate();
        Self {
            _guard: guard,
            slot,
            initiator,
        }
    }

    fn initiator(&self) -> usize {
        self.initiator
    }

    fn allocate_seq(&self) -> RequestSeq {
        self.slot.allocate_seq()
    }

    fn clear_targets(&self) {
        self.slot.clear_targets();
    }

    fn mark_target(&self, target_cpu: LogicalCpuId) {
        self.slot.mark_target(target_cpu);
    }

    fn publish(&self, request_seq: RequestSeq, vaddr: Option<VirtAddr>) {
        self.slot.publish(request_seq, vaddr);
    }

    fn is_acked_by(&self, cpu: usize, request_seq: RequestSeq) -> bool {
        self.slot.is_acked_by(cpu, request_seq)
    }

    fn targeted_snapshot(&self) -> [bool; kbuild_config::NR_CPUS] {
        self.slot.targeted_snapshot()
    }

    fn acked_snapshot(&self) -> [u64; kbuild_config::NR_CPUS] {
        self.slot.acked_snapshot()
    }
}

impl Drop for ActiveShootdownSlot<'_> {
    fn drop(&mut self) {
        self.slot.deactivate();
    }
}

/// Enables remote TLB shootdown after every CPU's interrupt services are ready.
///
/// Call once on the primary CPU after the CPU-service rendezvous, before
/// admitting runtime kernel mapping changes or starting user processes. Shared
/// kernel mappings must stay frozen from AP startup until this publication;
/// each AP flushes locally when installing the runtime kernel page table.
/// Before AP startup, local completion covers the sole running CPU.
pub fn mark_all_cpus_started() {
    ALL_CPUS_STARTED.store(true, Ordering::Release);
}

#[kiface::provide]
impl TlbFlushIf {
    fn flush_process_mask(vaddr: Option<VirtAddr>, target_mask: KCpuMask) {
        flush_remote(vaddr, target_mask);
    }

    fn flush_all_cpus(vaddr: Option<VirtAddr>) {
        flush_remote(vaddr, all_present_cpu_mask());
    }
}

fn all_present_cpu_mask() -> KCpuMask {
    let mut all_mask = KCpuMask::new();
    for_each_present_logical_cpu(|_, cpu, _| {
        all_mask.set(cpu.as_usize(), true);
    });
    all_mask
}

#[inline]
fn note_pending_for_cpu(target_cpu: LogicalCpuId) {
    PENDING_EPOCH_BY_CPU[target_cpu.as_usize()].fetch_add(1, Ordering::Release);
}

#[inline]
fn current_pending_epoch(cpu: usize) -> u64 {
    PENDING_EPOCH_BY_CPU[cpu].load(Ordering::Acquire)
}

#[inline]
fn last_handled_pending_epoch() -> u64 {
    LAST_HANDLED_PENDING_EPOCH.load_current()
}

#[inline]
fn set_last_handled_pending_epoch(epoch: u64) {
    // One local scalar store needs no transaction guard. Masking IRQs only
    // around this store would not prevent a nested scan before the store.
    LAST_HANDLED_PENDING_EPOCH.store_current(epoch);
}

/// Shared shootdown logic: publish `vaddr`, send IPIs to every CPU set in
/// `target_mask` (except self), and spin-wait for acknowledgement of this
/// exact request sequence. Runtime callers must be in task context with IRQs
/// enabled; hardirq, softirq and NMI callbacks must not initiate mapping changes.
/// Waiting pins this CPU but leaves IRQs enabled so incoming IPIs can run.
/// Targets must eventually re-enable IRQs without depending on another task
/// on this CPU or on locks retained by the initiator.
fn flush_remote(vaddr: Option<VirtAddr>, target_mask: KCpuMask) {
    if !ALL_CPUS_STARTED.load(Ordering::Acquire) {
        karch::flush_tlb(vaddr);
        return;
    }
    {
        let active_slot = ActiveShootdownSlot::acquire_current();
        karch::flush_tlb(vaddr);
        let my_cpu = this_cpu_id();

        let targets: [Option<LogicalCpuId>; kbuild_config::NR_CPUS] = {
            let mut buf = [None; kbuild_config::NR_CPUS];
            let mut idx = 0;
            for cpu in target_mask.iter_logical() {
                if cpu != my_cpu {
                    buf[idx] = Some(cpu);
                    idx += 1;
                }
            }
            buf
        };

        let request_seq = active_slot.allocate_seq();

        active_slot.clear_targets();
        for target in targets.iter().flatten() {
            active_slot.mark_target(*target);
        }
        active_slot.publish(request_seq, vaddr);

        if targets[0].is_some() {
            for target in targets.iter().flatten() {
                note_pending_for_cpu(*target);
            }
            // `notify_cpu()` is responsible for publish-before-notify ordering
            // on each architecture, so the target cannot observe the IPI
            // before this request becomes visible.
            for target in targets.iter().flatten() {
                kirq::notify_cpu(IPI_IRQ, TargetCpu::Specific(target.as_usize()));
            }

            let start = khal::time::monotonic_time();
            let mut warned = false;
            let mut last_stall_report = start;
            for target in targets.iter().flatten() {
                let mut last_resend = start;
                while !active_slot.is_acked_by(target.as_usize(), request_seq) {
                    let now = khal::time::monotonic_time();
                    let elapsed = now.saturating_duration_since(start);
                    // IRQs stay enabled so concurrent requests are acknowledged
                    // by the normal IPI handler. Retry a lost notification rather
                    // than returning before the target has completed this request.
                    if now.saturating_duration_since(last_resend) >= SHOOTDOWN_RESEND_INTERVAL {
                        last_resend = now;
                        kirq::notify_cpu(IPI_IRQ, TargetCpu::Specific(target.as_usize()));
                    }
                    if !warned && elapsed >= SHOOTDOWN_WARN {
                        warned = true;
                        warn!(
                            "tlb shootdown wait: initiator_cpu={} stuck_on_target_cpu={} \
                             flush_all={} vaddr={:?} request_seq={} targeted={:?} acked={:?}",
                            active_slot.initiator(),
                            target.as_usize(),
                            vaddr.is_none(),
                            vaddr,
                            request_seq.get(),
                            active_slot.targeted_snapshot(),
                            active_slot.acked_snapshot()
                        );
                    }
                    if now.saturating_duration_since(last_stall_report)
                        >= SHOOTDOWN_ESCALATE_TIMEOUT
                    {
                        last_stall_report = now;
                        error!(
                            "tlb shootdown STALL sustained: initiator_cpu={} target_cpu={} \
                             elapsed_ms={} request_seq={}; re-delivering but target still not \
                             acknowledging",
                            active_slot.initiator(),
                            target.as_usize(),
                            elapsed.into_core().as_millis() as u64,
                            request_seq.get()
                        );
                    }
                    core::hint::spin_loop();
                }
            }
        }
    }
}

/// Handle any pending TLB shootdown requests targeting the current CPU.
///
/// Called from the shared IPI interrupt handler. Runtime initiators leave IRQs
/// enabled while waiting so they can also acknowledge incoming requests.
///
/// The fast path checks this CPU's pending epoch first and returns immediately
/// when no TLB request has targeted the CPU since the last scan, so generic
/// IPI callbacks do not pay the full request-slot walk cost.
pub fn handle_shootdown() {
    let cpu: usize = this_cpu_id().into();
    let pending_epoch = current_pending_epoch(cpu);
    if pending_epoch == last_handled_pending_epoch() {
        return;
    }

    // Only scan slots belonging to present CPUs, skipping the tail of the
    // `NR_CPUS`-sized array that corresponds to non-existent CPUs.
    for_each_present_logical_cpu(|_, initiator, _| {
        let request_slot = &REQUEST_SLOTS[initiator.as_usize()];
        let Some(request) = request_slot.load_published_request() else {
            return;
        };
        if request_slot.is_acked_by(cpu, request.seq()) {
            return;
        }
        if !request.targets_cpu(cpu) {
            return;
        }

        if let Some(vaddr) = request.flush_vaddr() {
            karch::flush_tlb(Some(vaddr));
        } else {
            karch::flush_tlb(None);
        }

        #[cfg(unittest)]
        run_before_ack_hook_for_test();
        request_slot.ack(cpu, request.seq());
    });

    // Store the entry snapshot, not a post-scan re-read. If a new request
    // arrives during the scan, it will raise the epoch again and force another
    // scan on the next IPI instead of being accidentally absorbed here.
    set_last_handled_pending_epoch(pending_epoch);
}

/// Runs a serial integration test with a scoped hook before remote ACKs.
/// The hook must obey the IRQ-safe shootdown-handler contract. The previous
/// hook is restored on scope exit; this does not drain in-flight handlers.
#[cfg(unittest)]
pub fn with_before_ack_hook_for_test<R>(hook: fn(), operation: impl FnOnce() -> R) -> R {
    struct RestoreHook(usize);
    impl Drop for RestoreHook {
        fn drop(&mut self) {
            BEFORE_ACK_HOOK.store(self.0, Ordering::Release);
        }
    }
    let _restore = RestoreHook(BEFORE_ACK_HOOK.swap(hook as usize, Ordering::AcqRel));
    operation()
}

#[cfg(unittest)]
fn run_before_ack_hook_for_test() {
    let hook = BEFORE_ACK_HOOK.load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: only with_before_ack_hook_for_test stores static function
        // pointers or zero; restoring a prior hook cannot invalidate a pointer.
        let hook = unsafe { core::mem::transmute::<usize, fn()>(hook) };
        hook();
    }
}

/// Test helper: trigger a TLB shootdown directly through the kiface facade.
#[cfg(unittest)]
pub fn trigger_flush_all(vaddr: Option<VirtAddr>) {
    TlbFlushIf::flush_all_cpus(vaddr);
}

unittest::path_mod_test!(tlb_unittest, "../ut-tests/tlb_unittest.rs");
