// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deferred-probe pending/active queues and executor handoff.
//!
//! The queues own intrusive request nodes allocated before the deferred lock;
//! a trigger only splices existing lists and never allocates or drops a node.
//! The workqueue owns callback serialization, so one drain callback processes
//! active nodes one at a time and `claimed` is the sole identity of the node it
//! has removed.

use alloc::sync::Arc;
use core::mem;

use driver_base::DriverError;
use intrusive_collections::{Adapter, DefaultLinkOps, LinkOps, LinkedList, LinkedListLink};
use kalloc_types::{KBox, KBoxPointerOps};
use kspin::{SpinNoPreempt, static_lock};

use crate::DeviceDescId;

/// A fixed, bounded diagnostic reason for a deferred probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferReason {
    /// A driver explicitly reported that an external dependency is not ready.
    Dependency,
    /// The supplier began exiting; the waiter was cancelled.
    SupplierUnbinding,
    /// The supplier was removed; the waiter reached its terminal diagnosis.
    SupplierRemoved,
}

/// One queued deferred-probe request.
struct RequestNode {
    desc: DeviceDescId,
    request: u64,
    reason: DeferReason,
    candidate: u64,
    supplier: Option<crate::SupplierRef>,
    link: LinkedListLink,
}

type RequestPointerOps = KBoxPointerOps<RequestNode>;
const REQUEST_POINTER_OPS: RequestPointerOps = KBoxPointerOps::new();

/// Adapter joining KBox ownership to the intrusive linked list.
struct RequestAdapter {
    link_ops: <LinkedListLink as DefaultLinkOps>::Ops,
    pointer_ops: RequestPointerOps,
}

impl RequestAdapter {
    const NEW: Self = Self {
        link_ops: <LinkedListLink as DefaultLinkOps>::NEW,
        pointer_ops: REQUEST_POINTER_OPS,
    };
}

// SAFETY: the adapter carries no shared mutable state; queue access is
// serialized by the deferred spinlock.
unsafe impl Send for RequestAdapter {}
// SAFETY: see the Send justification above.
unsafe impl Sync for RequestAdapter {}

// SAFETY: intrusive-collections may call `get_link` only with a pointer to a
// live, initialized `RequestNode`; every node is uniquely owned by a KBox and,
// while linked, belongs to exactly one pending/active list protected by the
// SpinNoPreempt STATE lock. `get_link` returns the address of that node's
// embedded `link` field using its exact `offset_of!`; `get_value` subtracts the
// same offset, so the two operations are inverses and recover the original
// allocation. The adapter itself is stateless, and the list contract supplies
// the live/uniquely-linked precondition for every call.
unsafe impl Adapter for RequestAdapter {
    type LinkOps = <LinkedListLink as DefaultLinkOps>::Ops;
    type PointerOps = RequestPointerOps;

    unsafe fn get_value(&self, link: <Self::LinkOps as LinkOps>::LinkPtr) -> *const RequestNode {
        // SAFETY: intrusive-collections calls this only with a link pointer
        // previously produced by `get_link` for a live, list-owned RequestNode.
        // The node allocation is initialized and large enough to contain the
        // link field, so subtracting the exact field offset recovers that same
        // initialized allocation.
        unsafe { intrusive_collections::container_of!(link.as_ptr(), RequestNode, link) }
    }

    unsafe fn get_link(&self, value: *const RequestNode) -> <Self::LinkOps as LinkOps>::LinkPtr {
        debug_assert!(!value.is_null(), "adapter link value must be non-null");
        debug_assert_eq!(
            (value as usize) % core::mem::align_of::<RequestNode>(),
            0,
            "adapter link value must be aligned"
        );
        // SAFETY: the Adapter contract supplies a valid, non-null pointer to an
        // initialized live RequestNode. `offset_of` is the in-bounds byte
        // offset of its embedded link field, so adding it stays inside the same
        // allocation and cannot produce null.
        let ptr = unsafe {
            (value as *const u8).add(core::mem::offset_of!(RequestNode, link))
                as *mut LinkedListLink
        };
        // SAFETY: the field pointer computed above is in-bounds and non-null.
        unsafe { core::ptr::NonNull::new_unchecked(ptr) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.link_ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.link_ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointer_ops
    }
}

type RequestList = LinkedList<RequestAdapter>;

/// A request identity claimed by the single drain worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Claimed {
    desc: DeviceDescId,
    request: u64,
}

/// How this attempt interacts with an existing deferred request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptMode {
    /// Explicit probe: supersede any pending request before probing.
    Explicit,
    /// Only-driver scan: preserve the old request on no-match/ordinary failure.
    Limited,
    /// The drain worker: the claimed request is already current.
    Worker,
}

/// Identity of one probe attempt; it only protects queue takeover.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttemptToken {
    desc: DeviceDescId,
    start_trigger_seq: u64,
    claimed: Option<u64>,
    mode: AttemptMode,
}

impl AttemptToken {
    /// Limited scans must cancel an old request only after a successful bind.
    pub(crate) const fn cancel_old_on_success(&self) -> bool {
        matches!(self.mode, AttemptMode::Limited)
    }
}

/// Executor hook installed by the host integration. `kick` must be non-sleeping
/// and allocation-free; the drain callback itself may sleep.
pub trait DeferredExecutor: Send + Sync {
    /// Wake the single prebuilt drain work. Repeated calls may coalesce.
    fn kick(&self);
}

struct State {
    next_request: u64,
    trigger_seq: u64,
    pending: RequestList,
    active: RequestList,
    claimed: Option<Claimed>,
    executor: Option<Arc<dyn DeferredExecutor>>,
}

impl State {
    const fn new() -> Self {
        Self {
            next_request: 1,
            trigger_seq: 0,
            pending: RequestList::new(RequestAdapter::NEW),
            active: RequestList::new(RequestAdapter::NEW),
            claimed: None,
            executor: None,
        }
    }

    fn alloc_request(&mut self) -> Result<u64, DriverError> {
        let value = self.next_request;
        self.next_request = value.checked_add(1).ok_or(DriverError::NoMemory)?;
        Ok(value)
    }

    fn has_active(&self) -> bool {
        !self.active.is_empty()
    }

    fn remove_desc(&mut self, desc: DeviceDescId) -> Option<KBox<RequestNode>> {
        for list in [&mut self.pending, &mut self.active] {
            let mut cursor = list.cursor_mut();
            cursor.move_next();
            while !cursor.is_null() {
                if cursor.get().is_some_and(|node| node.desc == desc) {
                    return cursor.remove();
                }
                cursor.move_next();
            }
        }
        None
    }

    fn remove_supplier(&mut self, supplier: &crate::SupplierRef) -> RequestList {
        let mut removed = RequestList::new(RequestAdapter::NEW);
        for list in [&mut self.pending, &mut self.active] {
            let mut cursor = list.cursor_mut();
            cursor.move_next();
            while !cursor.is_null() {
                let matches = cursor.get().is_some_and(|node| {
                    node.supplier
                        .as_ref()
                        .is_some_and(|current| current.device_id() == supplier.device_id())
                });
                if matches {
                    if let Some(node) = cursor.remove() {
                        removed.push_back(node);
                    }
                } else {
                    cursor.move_next();
                }
            }
        }
        removed
    }

    fn install(&mut self, executor: Arc<dyn DeferredExecutor>) -> Option<bool> {
        if self.executor.is_some() {
            return None;
        }
        self.executor = Some(executor);
        self.bump_trigger().ok()?;
        self.promote_pending();
        Some(self.has_active())
    }

    fn promote_pending(&mut self) {
        let pending = mem::replace(&mut self.pending, RequestList::new(RequestAdapter::NEW));
        let mut cursor = self.active.cursor_mut();
        cursor.move_prev();
        cursor.splice_after(pending);
    }

    fn bump_trigger(&mut self) -> Result<(), DriverError> {
        self.trigger_seq = self
            .trigger_seq
            .checked_add(1)
            .ok_or(DriverError::BadState)?;
        Ok(())
    }
}

static_lock! {
    static STATE: SpinNoPreempt<State> = SpinNoPreempt::new(State::new());
}
#[cfg(unittest)]
static_lock! {
    static LAST_CANCEL: SpinNoPreempt<Option<(crate::DeviceId, DeferReason)>> =
        SpinNoPreempt::new(None);
}

#[cfg(unittest)]
/// Deterministic seams for exercising the claimed-request takeover window.
pub mod test_hooks {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::DeviceDescId;

    static PAUSE_ARMED: AtomicBool = AtomicBool::new(false);
    static PAUSED: AtomicBool = AtomicBool::new(false);
    static RELEASE: AtomicBool = AtomicBool::new(false);
    static DRAIN_RUNNING: AtomicBool = AtomicBool::new(false);
    static DRAIN_CURRENT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    static DRAIN_MAX: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    static CANCELS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    /// Arm one pause immediately after the worker claims an active request.
    pub fn arm_pause_after_claim() {
        PAUSE_ARMED.store(true, Ordering::SeqCst);
    }

    /// Wait until the worker has claimed a request and is paused.
    pub fn wait_until_paused() -> bool {
        for _ in 0..4_000_000 {
            if PAUSED.load(Ordering::SeqCst) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// Release a paused worker.
    pub fn release_pause() {
        RELEASE.store(true, Ordering::SeqCst);
    }

    /// Whether no pending/active/claimed deferred request remains.
    pub fn is_idle() -> bool {
        let state = super::STATE.lock();
        state.pending.is_empty() && state.active.is_empty() && state.claimed.is_none()
    }

    /// Diagnostic pending descriptor for test failures.
    pub fn pending_desc() -> Option<DeviceDescId> {
        let state = super::STATE.lock();
        let cursor = state.pending.front();
        cursor.get().map(|node| node.desc)
    }

    /// Diagnostic candidate id of the first pending request.
    pub fn pending_candidate() -> Option<u64> {
        let state = super::STATE.lock();
        let cursor = state.pending.front();
        cursor.get().map(|node| node.candidate)
    }

    /// Diagnostic counts for test failures.
    pub fn debug_counts() -> (bool, bool, bool) {
        let state = super::STATE.lock();
        (
            !state.pending.is_empty(),
            !state.active.is_empty(),
            state.claimed.is_some(),
        )
    }

    /// Whether the single drain callback is currently running.
    pub fn is_drain_running() -> bool {
        DRAIN_RUNNING.load(Ordering::SeqCst)
    }

    pub fn max_concurrent_drains() -> usize {
        DRAIN_MAX.load(Ordering::SeqCst)
    }

    pub fn cancel_count() -> usize {
        CANCELS.load(Ordering::SeqCst)
    }

    pub(crate) fn note_cancel() {
        CANCELS.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn note_drain_start() {
        DRAIN_RUNNING.store(true, Ordering::SeqCst);
        let current = DRAIN_CURRENT.fetch_add(1, Ordering::SeqCst) + 1;
        let mut max = DRAIN_MAX.load(Ordering::SeqCst);
        while current > max {
            match DRAIN_MAX.compare_exchange(max, current, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(observed) => max = observed,
            }
        }
    }

    pub(crate) fn note_drain_end() {
        DRAIN_CURRENT.fetch_sub(1, Ordering::SeqCst);
        DRAIN_RUNNING.store(false, Ordering::SeqCst);
    }

    pub(crate) fn pause_after_claim() {
        pause(PausePoint::Claim);
    }

    /// Commit interleaving points used by B3/B4.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum CommitPause {
        BeforeCommit,
        AfterCommit,
    }

    struct CommitHook {
        desc: DeviceDescId,
        point: CommitPause,
        callback: Arc<dyn Fn() + Send + Sync>,
    }

    kspin::static_lock! {
        static COMMIT_HOOK: super::SpinNoPreempt<Option<CommitHook>> =
            super::SpinNoPreempt::new(None);
    }

    /// Install a one-shot, descriptor-filtered commit hook. The callback runs
    /// outside the deferred lock in the probing task and may block.
    pub fn install_commit_hook(
        desc: DeviceDescId,
        point: CommitPause,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) {
        *COMMIT_HOOK.lock() = Some(CommitHook {
            desc,
            point,
            callback,
        });
    }

    pub fn clear_commit_hook() {
        *COMMIT_HOOK.lock() = None;
    }

    pub(crate) fn pause_commit(desc: DeviceDescId, point: CommitPause) {
        let callback = {
            let mut hook = COMMIT_HOOK.lock();
            match hook.as_ref() {
                Some(current) if current.desc == desc && current.point == point => {
                    hook.take().map(|hook| hook.callback)
                }
                _ => None,
            }
        };
        if let Some(callback) = callback {
            callback();
        }
    }

    enum PausePoint {
        Claim,
    }

    fn pause(point: PausePoint) {
        let _ = point;
        if !PAUSE_ARMED.swap(false, Ordering::SeqCst) {
            return;
        }
        PAUSED.store(true, Ordering::SeqCst);
        while !RELEASE.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        RELEASE.store(false, Ordering::SeqCst);
        PAUSED.store(false, Ordering::SeqCst);
    }
}

fn kick_executor(executor: Arc<dyn DeferredExecutor>) {
    executor.kick();
}

fn schedule_if_active() {
    let executor = {
        let state = STATE.lock();
        if !state.has_active() {
            return;
        }
        state.executor.clone()
    };
    if let Some(executor) = executor {
        kick_executor(executor);
    }
}

fn begin(desc: DeviceDescId, mode: AttemptMode, claimed: Option<u64>) -> AttemptToken {
    let state = STATE.lock();
    AttemptToken {
        desc,
        start_trigger_seq: state.trigger_seq,
        claimed,
        mode,
    }
}

/// Start an explicit probe attempt and supersede any prior deferred request.
pub(crate) fn begin_explicit(desc: DeviceDescId) -> AttemptToken {
    let (token, old) = {
        let mut state = STATE.lock();
        let token = AttemptToken {
            desc,
            start_trigger_seq: state.trigger_seq,
            claimed: None,
            mode: AttemptMode::Explicit,
        };
        let old = state.remove_desc(desc);
        if state.claimed.is_some_and(|claimed| claimed.desc == desc) {
            state.claimed = None;
        }
        (token, old)
    };
    // Superseded request nodes are destroyed outside the deferred lock.
    drop(old);
    token
}

/// Start a limited (Only-driver) attempt without disturbing the old request.
pub(crate) fn begin_limited(desc: DeviceDescId) -> AttemptToken {
    begin(desc, AttemptMode::Limited, None)
}

/// Start the worker attempt for an already claimed request.
pub(crate) fn begin_worker(desc: DeviceDescId, request: u64) -> AttemptToken {
    begin(desc, AttemptMode::Worker, Some(request))
}

/// Commit a deferred request after the failed binding has fully retired.
///
/// The caller holds the descriptor binding lock, so a successful commit cannot
/// race descriptor deletion; no separate `is_pending` snapshot is needed.
pub(crate) fn commit_deferred(
    attempt: AttemptToken,
    reason: DeferReason,
    candidate: u64,
    supplier: Option<crate::SupplierRef>,
) -> Result<u64, DriverError> {
    let request = {
        let mut state = STATE.lock();
        if supplier
            .as_ref()
            .is_some_and(|supplier| supplier.is_unregistered())
        {
            return Err(DriverError::ResourceBusy);
        }
        state.alloc_request()?
    };
    let node = KBox::new(RequestNode {
        desc: attempt.desc,
        request,
        reason,
        candidate,
        supplier: supplier.clone(),
        link: LinkedListLink::new(),
    })
    .map_err(|_| DriverError::NoMemory)?;

    let mut state = STATE.lock();
    if supplier
        .as_ref()
        .is_some_and(|supplier| supplier.is_unregistered())
    {
        return Err(DriverError::ResourceBusy);
    }
    let mut old = None;
    match attempt.mode {
        AttemptMode::Explicit => {}
        AttemptMode::Limited => {
            // A limited deferral replaces the prior request AND must invalidate
            // any worker that already claimed that request.
            old = state.remove_desc(attempt.desc);
            if state
                .claimed
                .is_some_and(|claimed| claimed.desc == attempt.desc)
            {
                state.claimed = None;
            }
        }
        AttemptMode::Worker => {
            old = state.remove_desc(attempt.desc);
            if state.claimed.is_some_and(|claimed| {
                claimed.desc == attempt.desc
                    && attempt.claimed.is_some_and(|old| old == claimed.request)
            }) {
                state.claimed = None;
            }
        }
    }
    let missed = state.trigger_seq != attempt.start_trigger_seq;
    if missed {
        state.active.push_back(node);
    } else {
        state.pending.push_back(node);
    }
    drop(state);
    drop(old);
    #[cfg(unittest)]
    test_hooks::pause_commit(attempt.desc, test_hooks::CommitPause::AfterCommit);

    if missed {
        schedule_if_active();
    }
    Ok(request)
}

/// Cancel every waiter targeting a supplier that is exiting.
pub(crate) fn cancel_for_supplier(supplier: &crate::SupplierRef, reason: DeferReason) -> usize {
    #[cfg(not(unittest))]
    let _ = reason;
    let removed = {
        let mut state = STATE.lock();
        #[cfg(unittest)]
        {
            *LAST_CANCEL.lock() = Some((supplier.device_id(), reason));
        }
        state.remove_supplier(supplier)
    };
    let count = removed.iter().count();
    // Unlink is complete; node destruction happens after F is released.
    drop(removed);
    count
}

/// Last supplier-exit cancellation diagnosis, for deterministic tests.
#[cfg(unittest)]
pub fn last_cancel() -> Option<(crate::DeviceId, DeferReason)> {
    *LAST_CANCEL.lock()
}

/// Cancel every pending/active request and claimed identity for one descriptor.
pub(crate) fn cancel_waits(desc: DeviceDescId) {
    #[cfg(unittest)]
    test_hooks::note_cancel();
    let removed = {
        let mut state = STATE.lock();
        let removed = state.remove_desc(desc);
        if state.claimed.is_some_and(|claimed| claimed.desc == desc) {
            state.claimed = None;
        }
        removed
    };
    // Node destruction is outside the deferred lock.
    drop(removed);
}

/// Finish a claimed request only if it is still the current claim.
pub(crate) fn finish_claimed(desc: DeviceDescId, request: u64) {
    let mut state = STATE.lock();
    if state
        .claimed
        .is_some_and(|claimed| claimed.desc == desc && claimed.request == request)
    {
        state.claimed = None;
    }
}

/// Whether `request` is still the worker's current claim.
pub(crate) fn is_claimed(desc: DeviceDescId, request: u64) -> bool {
    STATE
        .lock()
        .claimed
        .is_some_and(|claimed| claimed.desc == desc && claimed.request == request)
}

/// Remove the next active request and publish it as the current claim.
fn take_active() -> Option<(DeviceDescId, u64)> {
    let (desc, request, reason, candidate, node) = {
        let mut state = STATE.lock();
        let node = state.active.pop_front()?;
        let desc = node.desc;
        let request = node.request;
        let reason = node.reason;
        let candidate = node.candidate;
        state.claimed = Some(Claimed { desc, request });
        (desc, request, reason, candidate, node)
    };
    // Node destruction is outside the deferred lock; diagnostics are bounded.
    log::debug!(
        "deferred drain took desc {:?} request {} reason {:?} candidate {}",
        desc,
        request,
        reason,
        candidate,
    );
    drop(node);
    #[cfg(unittest)]
    test_hooks::pause_after_claim();
    Some((desc, request))
}

/// Install the fixed-lifetime executor once and drain anything already pending.
pub fn install_executor(executor: Arc<dyn DeferredExecutor>) {
    let kick = {
        let mut state = STATE.lock();
        match state.install(executor) {
            Some(kick) => kick,
            None => {
                drop(state);
                return;
            }
        }
    };
    if kick {
        schedule_if_active();
    }
}

#[cfg(unittest)]
/// Isolated state contract: pending requests are accepted before an executor
/// is installed and become active when installation occurs.
pub fn backlog_contract_for_test() -> bool {
    struct FakeExecutor;
    impl DeferredExecutor for FakeExecutor {
        fn kick(&self) {}
    }

    let mut empty = State::new();
    let empty_seq = empty.trigger_seq;
    if empty.install(Arc::new(FakeExecutor)) != Some(false)
        || empty.trigger_seq == empty_seq
        || empty.has_active()
    {
        return false;
    }

    let mut state = State::new();
    let Ok(request) = state.alloc_request() else {
        return false;
    };
    let Ok(node) = KBox::new(RequestNode {
        desc: DeviceDescId::new(1),
        request,
        reason: DeferReason::Dependency,
        candidate: 1,
        supplier: None,
        link: LinkedListLink::new(),
    }) else {
        return false;
    };
    state.pending.push_back(node);
    if state.has_active() {
        return false;
    }
    let before = state.trigger_seq;
    let installed = state.install(Arc::new(FakeExecutor));
    if installed != Some(true) || !state.pending.is_empty() || !state.has_active() {
        return false;
    }
    if state.trigger_seq == before {
        return false;
    }
    // A second installation is a no-op and must not replace the executor.
    state.install(Arc::new(FakeExecutor)).is_none()
}

#[cfg(unittest)]
/// Active-before-pending FIFO order must survive promotion.
pub fn promote_order_for_test() -> bool {
    let mut state = State::new();
    let make = |request| {
        KBox::new(RequestNode {
            desc: DeviceDescId::new(request),
            request,
            reason: DeferReason::Dependency,
            candidate: request,
            supplier: None,
            link: LinkedListLink::new(),
        })
        .ok()
    };
    let (Some(a), Some(b)) = (make(1), make(2)) else {
        return false;
    };
    state.active.push_back(a);
    state.pending.push_back(b);
    state.promote_pending();
    let first = state.active.pop_front().map(|node| node.request);
    let second = state.active.pop_front().map(|node| node.request);
    first == Some(1) && second == Some(2)
}

/// Whether the fixed-lifetime executor has been installed.
pub fn executor_installed() -> bool {
    STATE.lock().executor.is_some()
}

#[cfg(unittest)]
/// Cancel every deferred request before a test fixture reset.
pub fn cancel_all_for_tests() {
    let old = {
        let mut state = STATE.lock();
        state.claimed = None;
        (
            mem::replace(&mut state.pending, RequestList::new(RequestAdapter::NEW)),
            mem::replace(&mut state.active, RequestList::new(RequestAdapter::NEW)),
        )
    };
    drop(old);
}

#[cfg(unittest)]
/// Clear deferred bookkeeping between test cases after the drain is quiescent.
pub fn reset_for_tests() {
    let old = {
        let mut state = STATE.lock();
        state.claimed = None;
        state.trigger_seq = 0;
        (
            mem::replace(&mut state.pending, RequestList::new(RequestAdapter::NEW)),
            mem::replace(&mut state.active, RequestList::new(RequestAdapter::NEW)),
        )
    };
    drop(old);
}

/// Successful binding completed; advance all pending requests once.
pub(crate) fn notify_success() {
    let kick = {
        let mut state = STATE.lock();
        if state.bump_trigger().is_err() {
            return;
        }
        state.promote_pending();
        state.has_active()
    };
    if kick {
        schedule_if_active();
    }
}

/// Worker body: consume active requests until none remain.
pub fn drain() {
    #[cfg(unittest)]
    test_hooks::note_drain_start();
    while let Some((desc, request)) = take_active() {
        let result = super::retry_deferred_request(desc, request);
        // Every exit path, including early registry/lock errors, must retire
        // the matching claim; a newer claim is deliberately left untouched.
        finish_claimed(desc, request);
        if let Err(error) = result {
            log::warn!(
                "deferred retry for descriptor {:?} failed: {:?}",
                desc,
                error
            );
        }
    }
    #[cfg(unittest)]
    test_hooks::note_drain_end();
}
