// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit consumer -> supplier dependency ledger for exit ordering.
//!
//! The public probe API takes a `SupplierRef`, so dependency declaration keeps
//! the stable device object alive. Internal keys use DeviceId plus the exact
//! binding instance; the ledger never resolves an ID to an object under its
//! own spinlock.

use alloc::sync::Arc;

use driver_base::DriverError;
use kspin::{SpinNoPreempt, static_lock};

use crate::{BindingRef, DeviceId, ProbeContext, SupplierRef};

/// Result of declaring a dependency from a probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DependencyResult {
    /// Supplier is bound and the edge is in CONSUMER_PROBE.
    Ready,
    /// Supplier exists but is not ready; the probe must return Deferred.
    Waiting,
}

/// Result of attempting to activate a consumer's edges after probe success.
pub(crate) enum DependencyOutcome {
    /// All dependencies remain current and the edges are ACTIVE.
    Activated,
    /// A supplier began exiting; the caller must roll back and wait on it.
    Deferred(SupplierRef),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EdgeState {
    Dormant,
    ConsumerProbe,
    Active,
    SupplierUnbind,
}

struct Edge {
    consumer: DeviceId,
    // CONSUMER_PROBE/ACTIVE only. DORMANT waiters never retain a rolled-back
    // attempt binding; the next probe claims the edge with a new generation.
    consumer_binding: Option<BindingRef>,
    supplier: SupplierRef,
    supplier_binding: Option<BindingRef>,
    state: EdgeState,
    pre_exit: Option<EdgeState>,
}

impl Edge {
    fn supplier_id(&self) -> DeviceId {
        self.supplier.device_id()
    }
}

const EDGE_SLOTS: usize = 64;

struct Ledger {
    slots: [Option<Edge>; EDGE_SLOTS],
}

impl Ledger {
    const fn new() -> Self {
        Self {
            slots: [const { None }; EDGE_SLOTS],
        }
    }
}

/// Sole owner of dependency edges and supplier-exit markers.
pub(crate) struct DependencyLedger {
    edges: Ledger,
}

impl DependencyLedger {
    const fn new() -> Self {
        Self {
            edges: Ledger::new(),
        }
    }

    fn take_where(&mut self, remove: impl Fn(&Edge) -> bool) -> [Option<Edge>; EDGE_SLOTS] {
        let mut removed = [const { None }; EDGE_SLOTS];
        let mut removed_count = 0;
        for slot in self.edges.slots.iter_mut() {
            if let Some(edge) = slot.as_ref()
                && remove(edge)
            {
                removed[removed_count] = slot.take();
                removed_count += 1;
            }
        }
        removed
    }
}

static_lock! {
    static LEDGER: SpinNoPreempt<DependencyLedger> = SpinNoPreempt::new(DependencyLedger::new());
}
static_lock! {
    static EXITING: SpinNoPreempt<[Option<DeviceId>; EDGE_SLOTS]> =
        SpinNoPreempt::new([None; EDGE_SLOTS]);
}

#[cfg(unittest)]
/// One-shot pause point after supplier-exit collection, outside all locks.
pub mod test_hooks {
    use alloc::sync::Arc;

    use kspin::SpinNoPreempt;

    use crate::DeviceId;

    type Callback = Arc<dyn Fn() + Send + Sync>;
    kspin::static_lock! {
        static UNBIND_HOOK: SpinNoPreempt<Option<(DeviceId, Callback)>> =
            SpinNoPreempt::new(None);
    }

    pub fn install_unbind_hook(supplier: DeviceId, callback: Callback) {
        *UNBIND_HOOK.lock() = Some((supplier, callback));
    }

    pub fn clear_unbind_hook() {
        *UNBIND_HOOK.lock() = None;
    }

    kspin::static_lock! {
        static DROP_HOOK: SpinNoPreempt<Option<Callback>> = SpinNoPreempt::new(None);
    }

    pub fn install_drop_hook(callback: Callback) {
        *DROP_HOOK.lock() = Some(callback);
    }

    pub fn clear_drop_hook() {
        *DROP_HOOK.lock() = None;
    }

    pub(crate) fn pause_after_unlink() {
        let callback = DROP_HOOK.lock().take();
        if let Some(callback) = callback {
            callback();
        }
    }

    pub(crate) fn pause_before_unbind(supplier: DeviceId) {
        let callback = {
            let mut hook = UNBIND_HOOK.lock();
            match hook.as_ref() {
                Some((current, _)) if *current == supplier => {
                    hook.take().map(|(_, callback)| callback)
                }
                _ => None,
            }
        };
        if let Some(callback) = callback {
            callback();
        }
    }
}

fn clear_exiting(supplier: DeviceId) {
    let mut exiting = EXITING.lock();
    for slot in exiting.iter_mut() {
        if *slot == Some(supplier) {
            *slot = None;
        }
    }
}

fn remove_edges(remove: impl Fn(&Edge) -> bool) {
    let removed = LEDGER.lock().take_where(remove);
    #[cfg(unittest)]
    test_hooks::pause_after_unlink();
    drop(removed);
}

fn same_binding(a: &BindingRef, b: &BindingRef) -> bool {
    Arc::ptr_eq(a.runtime(), b.runtime())
}

/// Declare one explicit dependency from the current probe binding.
pub(crate) fn add(
    ctx: &ProbeContext<'_>,
    supplier: SupplierRef,
) -> Result<DependencyResult, DriverError> {
    let consumer = ctx.device().id();
    let consumer_binding = ctx.binding().clone();
    let supplier_id = supplier.device_id();

    let exiting = EXITING.lock();
    let mut ledger = LEDGER.lock();
    // Revalidate the captured generation under both guards: a supplier exit
    // may have completed between SupplierRef creation and this insertion.
    let supplier_binding = supplier.binding_ref();
    if let Some(binding) = supplier_binding.as_ref()
        && binding.state() != crate::DeviceState::Active
    {
        if binding.state() == crate::DeviceState::Removed {
            return Err(DriverError::InvalidInput);
        }
        return Ok(DependencyResult::Waiting);
    }
    if exiting
        .iter()
        .flatten()
        .any(|current| *current == supplier_id)
    {
        return Ok(DependencyResult::Waiting);
    }
    for slot in ledger.edges.slots.iter_mut().flatten() {
        if slot.consumer == consumer && slot.supplier_id() == supplier_id {
            if slot.state == EdgeState::SupplierUnbind {
                return Ok(DependencyResult::Waiting);
            }
            slot.consumer_binding = Some(consumer_binding);
            slot.supplier_binding = supplier_binding;
            slot.state = if slot.supplier_binding.is_some() {
                EdgeState::ConsumerProbe
            } else {
                EdgeState::Dormant
            };
            return Ok(if slot.state == EdgeState::ConsumerProbe {
                DependencyResult::Ready
            } else {
                DependencyResult::Waiting
            });
        }
    }
    let state = if supplier_binding.is_some() {
        EdgeState::ConsumerProbe
    } else {
        EdgeState::Dormant
    };
    let edge = Edge {
        consumer,
        consumer_binding: Some(consumer_binding),
        supplier,
        supplier_binding,
        state,
        pre_exit: None,
    };
    if let Some(slot) = ledger.edges.slots.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(edge);
    } else {
        return Err(DriverError::NoMemory);
    }
    Ok(if state == EdgeState::ConsumerProbe {
        DependencyResult::Ready
    } else {
        DependencyResult::Waiting
    })
}

/// Activate all CONSUMER_PROBE edges for one consumer binding.
pub(crate) fn probe_succeeded(binding: &BindingRef) -> DependencyOutcome {
    let mut ledger = LEDGER.lock();
    let mut deferred = None;
    for edge in ledger.edges.slots.iter_mut().flatten() {
        if edge
            .consumer_binding
            .as_ref()
            .is_none_or(|current| !same_binding(current, binding))
        {
            continue;
        }
        match edge.state {
            EdgeState::ConsumerProbe => edge.state = EdgeState::Active,
            EdgeState::SupplierUnbind => deferred = Some(edge.supplier.clone()),
            EdgeState::Active | EdgeState::Dormant => {}
        }
    }
    match deferred {
        Some(supplier) => DependencyOutcome::Deferred(supplier),
        None => DependencyOutcome::Activated,
    }
}

/// Drop all unfinished edges for a consumer probe that failed normally.
pub(crate) fn probe_failed(binding: &BindingRef) {
    remove_edges(|edge| {
        edge.consumer_binding
            .as_ref()
            .is_some_and(|current| same_binding(current, binding))
            && edge.state != EdgeState::Active
    });
}

/// Drop all edges for a consumer binding during unbind/rebind.
pub(crate) fn consumer_unbind(binding: &BindingRef) {
    remove_edges(|edge| {
        edge.consumer_binding
            .as_ref()
            .is_some_and(|current| same_binding(current, binding))
    });
}

/// Drop every edge belonging to a consumer that disappeared concurrently.
pub(crate) fn forget_consumer(consumer: DeviceId) {
    remove_edges(|edge| edge.consumer == consumer);
}

/// Record the committed candidate's DORMANT waiter by stable identities only.
pub(crate) fn remember_waiter(
    consumer: DeviceId,
    supplier: SupplierRef,
) -> Result<(), DriverError> {
    let supplier_id = supplier.device_id();
    let exiting = EXITING.lock();
    let mut ledger = LEDGER.lock();
    if exiting
        .iter()
        .flatten()
        .any(|current| *current == supplier_id)
        || supplier.is_unregistered()
    {
        return Err(DriverError::ResourceBusy);
    }
    for slot in ledger.edges.slots.iter_mut().flatten() {
        if slot.consumer == consumer && slot.supplier_id() == supplier_id {
            slot.consumer_binding = None;
            slot.supplier_binding = None;
            slot.state = EdgeState::Dormant;
            return Ok(());
        }
    }
    let edge = Edge {
        consumer,
        consumer_binding: None,
        supplier,
        supplier_binding: None,
        state: EdgeState::Dormant,
        pre_exit: None,
    };
    if let Some(slot) = ledger.edges.slots.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(edge);
        Ok(())
    } else {
        Err(DriverError::NoMemory)
    }
}

/// Mark every edge for `supplier` SUPPLIER_UNBIND and return ACTIVE consumers.
pub(crate) fn begin_supplier_exit(supplier: DeviceId) -> [Option<DeviceId>; EDGE_SLOTS] {
    let mut exiting = EXITING.lock();
    if let Some(slot) = exiting.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(supplier);
    }
    let mut ledger = LEDGER.lock();
    let mut consumers = [None; EDGE_SLOTS];
    let mut consumer_index = 0;
    for edge in ledger
        .edges
        .slots
        .iter_mut()
        .flatten()
        .filter(|edge| edge.supplier_id() == supplier)
    {
        if edge.pre_exit.is_none() {
            edge.pre_exit = Some(edge.state);
        }
        match edge.state {
            EdgeState::Active | EdgeState::ConsumerProbe => {
                consumers[consumer_index] = Some(edge.consumer);
                consumer_index += 1;
            }
            EdgeState::Dormant => edge.supplier_binding = None,
            EdgeState::SupplierUnbind => {}
        }
        edge.state = EdgeState::SupplierUnbind;
    }
    consumers
}

/// A consumer could not be unbound; restore edges so the supplier stays live.
pub(crate) fn rollback_supplier_exit(supplier: DeviceId, supplier_binding: Option<BindingRef>) {
    let mut ledger = LEDGER.lock();
    for edge in ledger
        .edges
        .slots
        .iter_mut()
        .flatten()
        .filter(|edge| edge.supplier_id() == supplier)
    {
        if edge.state == EdgeState::SupplierUnbind {
            let previous = edge.pre_exit.take().unwrap_or(EdgeState::Dormant);
            edge.supplier_binding = if previous == EdgeState::Dormant {
                None
            } else {
                supplier_binding.clone()
            };
            edge.state = previous;
        }
    }
    drop(ledger);
    clear_exiting(supplier);
}

/// Delete all edges for a supplier after its removal committed.
pub(crate) fn finish_supplier_exit(supplier: DeviceId) {
    remove_edges(|edge| edge.supplier_id() == supplier);
    clear_exiting(supplier);
}

#[cfg(unittest)]
#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: test-only lock-order assertion"
)]
/// Detect registry lookup attempted while the ledger spinlock is held.
pub(crate) fn assert_registry_lookup_allowed() {
    if let Some(guard) = LEDGER.try_lock() {
        drop(guard);
    } else {
        panic!("registry lookup attempted under dependency ledger lock");
    }
}

#[cfg(unittest)]
pub fn forget_consumer_for_tests(consumer: DeviceId) {
    forget_consumer(consumer);
}

#[cfg(unittest)]
pub fn edge_count_for_tests(consumer: DeviceId, supplier: DeviceId) -> usize {
    LEDGER
        .lock()
        .edges
        .slots
        .iter()
        .flatten()
        .filter(|edge| edge.consumer == consumer && edge.supplier_id() == supplier)
        .count()
}

#[cfg(unittest)]
/// Drop all edges between kernel unit-test fixtures.
pub fn reset_for_tests() {
    let mut old = [const { None }; EDGE_SLOTS];
    {
        let mut ledger = LEDGER.lock();
        core::mem::swap(&mut ledger.edges.slots, &mut old);
    }
    drop(old);
    for slot in EXITING.lock().iter_mut() {
        *slot = None;
    }
}
