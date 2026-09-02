// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted network data-plane poller.

use kerrno::{KError, KResult};
use klazy::lazy_static;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ktime_types::TimeSpan;

use crate::SERVICE;

const DEFAULT_BACKGROUND_BUDGET: PollBudget = PollBudget {
    rx_packets: 512,
    tx_packets: 256,
    timer_events: 32,
};

const DEFAULT_ASSIST_BUDGET: PollBudget = PollBudget {
    rx_packets: 16,
    tx_packets: 16,
    timer_events: 8,
};

const DEFAULT_LEASE_BUDGET: PollBudget = PollBudget {
    rx_packets: 64,
    tx_packets: 64,
    timer_events: 8,
};

const LEASE_TIMEOUT: TimeSpan = TimeSpan::from_micros(500);

const BACKGROUND_MAX_ROUNDS: usize = 4;

lazy_static! {
    static ref NETWORK_POLLER: NetworkPoller = NetworkPoller::new(NetworkPollerConfig {
        background_budget: DEFAULT_BACKGROUND_BUDGET,
        lease_budget: DEFAULT_LEASE_BUDGET,
        assist_budget: DEFAULT_ASSIST_BUDGET,
        lease_timeout: LEASE_TIMEOUT,
    });
}

#[derive(Clone, Copy)]
pub(crate) enum PollReason {
    Rx,
    RxWindow,
    Tx,
    Timer,
}

#[derive(Clone, Copy)]
pub(crate) struct PollBudget {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
}

#[derive(Default)]
pub(crate) struct PollProgress {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
    /// Whether this round increased data TX queue capacity.
    pub tx_capacity_changed: bool,
    /// Whether work is ready for immediate processing in another round.
    ///
    /// A future protocol timer deadline does not count as immediate work.
    pub has_more: bool,
}

pub(crate) struct NetworkPollerConfig {
    background_budget: PollBudget,
    lease_budget: PollBudget,
    assist_budget: PollBudget,
    lease_timeout: TimeSpan,
}

type NetworkBudgetedPoller =
    kwork::BudgetedPoller<PollBudget, fn(PollBudget) -> kwork::BudgetedPollProgress>;
type NetworkPollLease = kwork::PollLease<PollBudget, fn(PollBudget) -> kwork::BudgetedPollProgress>;

pub(crate) struct NetworkPoller {
    tx_waiters: PollSet,
    poller: NetworkBudgetedPoller,
}

/// Task-context ownership for advancing the network poller.
pub(crate) struct NetAdvanceLease<'a> {
    owner: &'a NetworkPoller,
    inner: NetworkPollLease,
}

impl NetworkPoller {
    fn new(config: NetworkPollerConfig) -> Self {
        Self::new_with_poll_fn(config, poll_network_once)
    }

    fn new_with_poll_fn(
        config: NetworkPollerConfig,
        poll_once: fn(PollBudget) -> kwork::BudgetedPollProgress,
    ) -> Self {
        Self {
            tx_waiters: PollSet::new(),
            poller: kwork::BudgetedPoller::new_with_lease(
                "knet-poller",
                config.background_budget,
                config.lease_budget,
                config.assist_budget,
                config.lease_timeout,
                BACKGROUND_MAX_ROUNDS,
                poll_once,
            ),
        }
    }

    /// Starts the background workqueue during network initialization.
    pub(crate) fn start(&self) {
        if let Err(err) = self.poller.start() {
            warn!("failed to start knet-poller workqueue: {err:?}");
        }
    }

    /// Publishes network work and queues the background worker when it is idle.
    ///
    /// This method may run in IRQ-adjacent paths and does not acquire a sleepable
    /// lock or execute the network data plane.
    pub(crate) fn notify(&self, reason: PollReason) {
        let _ = reason;
        let _ = self.poller.notify_irq_safe();
    }

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.tx_waiters)
    }

    /// Assists one already scheduled round from the current task context.
    pub(crate) fn assist_once(&self) {
        self.poller.assist_once();
    }

    /// Attempts to transfer foreground progress responsibility to the caller.
    pub(crate) fn try_advance_lease(&self) -> Option<NetAdvanceLease<'_>> {
        self.poller
            .try_lease()
            .map(|inner| NetAdvanceLease { owner: self, inner })
    }

    /// Restores background progress for an expired foreground lease.
    pub(crate) fn kick_if_stale(&self) {
        self.poller.kick_if_stale();
    }

    fn run_once(&self, budget: PollBudget) -> PollProgress {
        if !SERVICE.is_inited() {
            return PollProgress::default();
        }

        let progress = SERVICE.poll_budgeted(budget);
        if progress.tx_capacity_changed {
            self.tx_waiters.wake();
        }
        debug_assert!(progress.rx_packets <= budget.rx_packets);
        debug_assert!(progress.tx_packets <= budget.tx_packets);
        debug_assert!(progress.timer_events <= budget.timer_events);
        progress
    }
}

impl NetAdvanceLease<'_> {
    /// Publishes network work and advances one lease-budgeted round.
    pub(crate) fn advance(&self, reason: PollReason) {
        self.owner.notify(reason);
        self.inner.poll_once();
    }
}

pub(crate) fn network_poller() -> &'static NetworkPoller {
    &NETWORK_POLLER
}

/// Assists one already scheduled network polling round from task context.
///
/// The call does not publish new work or wait for another executor.
pub(crate) fn assist_once() {
    network_poller().assist_once();
}

/// Attempts to acquire task-context ownership of network advancement.
#[allow(dead_code)]
pub(crate) fn try_advance_lease() -> Option<NetAdvanceLease<'static>> {
    network_poller().try_advance_lease()
}

/// Advances with a held lease or assists one existing scheduled round.
#[allow(dead_code)]
pub(crate) fn advance_or_assist(lease: Option<&NetAdvanceLease<'_>>, reason: PollReason) {
    advance_or_assist_with(network_poller(), lease, reason)
}

fn advance_or_assist_with(
    poller: &NetworkPoller,
    lease: Option<&NetAdvanceLease<'_>>,
    reason: PollReason,
) {
    if let Some(active_lease) = lease {
        debug_assert!(core::ptr::eq(active_lease.owner, poller));
        active_lease.advance(reason);
    } else {
        poller.assist_once();
    }
}

/// Attempts I/O first and acquires a lease only when the operation would block.
pub(crate) fn try_io_or_advance<T>(
    lease: &mut Option<NetAdvanceLease<'_>>,
    reason: PollReason,
    mut attempt: impl FnMut() -> KResult<T>,
) -> KResult<T> {
    try_io_or_advance_with(network_poller(), lease, reason, &mut attempt)
}

fn try_io_or_advance_with<'a, T>(
    poller: &'a NetworkPoller,
    lease: &mut Option<NetAdvanceLease<'a>>,
    reason: PollReason,
    mut attempt: impl FnMut() -> KResult<T>,
) -> KResult<T> {
    try_io_or_advance_with_lease(
        poller,
        lease,
        reason,
        || poller.try_advance_lease(),
        &mut attempt,
    )
}

fn try_io_or_advance_with_lease<'a, T>(
    poller: &'a NetworkPoller,
    lease: &mut Option<NetAdvanceLease<'a>>,
    reason: PollReason,
    mut try_lease: impl FnMut() -> Option<NetAdvanceLease<'a>>,
    mut attempt: impl FnMut() -> KResult<T>,
) -> KResult<T> {
    match attempt() {
        Err(KError::WouldBlock) => {
            if lease.is_none() {
                *lease = try_lease();
            }
            advance_or_assist_with(poller, lease.as_ref(), reason);
            release_lease_on_block(lease, attempt())
        }
        result => result,
    }
}

/// Releases foreground progress ownership before an I/O wait begins.
pub(crate) fn release_lease_on_block<T>(
    lease: &mut Option<NetAdvanceLease<'_>>,
    result: KResult<T>,
) -> KResult<T> {
    if matches!(result, Err(KError::WouldBlock)) {
        lease.take();
    }
    result
}

fn poll_network_once(budget: PollBudget) -> kwork::BudgetedPollProgress {
    kwork::BudgetedPollProgress {
        has_more: NETWORK_POLLER.run_once(budget).has_more,
    }
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    static POLL_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn test_poll_once(_budget: PollBudget) -> kwork::BudgetedPollProgress {
        POLL_CALLS.fetch_add(1, Ordering::AcqRel);
        kwork::BudgetedPollProgress { has_more: false }
    }

    fn test_poller() -> NetworkPoller {
        NetworkPoller::new_with_poll_fn(
            NetworkPollerConfig {
                background_budget: PollBudget {
                    rx_packets: 8,
                    tx_packets: 8,
                    timer_events: 2,
                },
                lease_budget: PollBudget {
                    rx_packets: 4,
                    tx_packets: 4,
                    timer_events: 1,
                },
                assist_budget: PollBudget {
                    rx_packets: 1,
                    tx_packets: 1,
                    timer_events: 1,
                },
                lease_timeout: TimeSpan::from_micros(500),
            },
            test_poll_once,
        )
    }

    #[def_test(serial)]
    fn would_block_releases_lease_and_requeues_published_work() {
        let poller = test_poller();
        let inner = poller.poller.acquire_lease_at_for_tests(u64::MAX).unwrap();
        let mut lease = Some(NetAdvanceLease {
            owner: &poller,
            inner,
        });
        poller.notify(PollReason::Tx);

        let result: KResult<()> = release_lease_on_block(&mut lease, Err(KError::WouldBlock));

        assert!(matches!(result, Err(KError::WouldBlock)));
        assert!(lease.is_none());
        assert_eq!(poller.poller.background_queue_attempts_for_tests(), 1);
    }

    #[def_test(serial)]
    fn advance_or_assist_uses_lease_and_releases_it_on_would_block() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = test_poller();
        let inner = poller.poller.acquire_lease_at_for_tests(u64::MAX).unwrap();
        let mut lease = Some(NetAdvanceLease {
            owner: &poller,
            inner,
        });

        advance_or_assist_with(&poller, lease.as_ref(), PollReason::Rx);
        let result: KResult<()> = release_lease_on_block(&mut lease, Err(KError::WouldBlock));

        assert!(matches!(result, Err(KError::WouldBlock)));
        assert!(lease.is_none());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 1);
    }

    #[def_test(serial)]
    fn advance_or_assist_without_lease_uses_scheduled_assist() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = test_poller();
        poller.notify(PollReason::Rx);
        advance_or_assist_with(&poller, None, PollReason::Rx);

        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 1);
    }

    #[def_test]
    fn ready_io_does_not_acquire_lease_or_advance_poller() {
        static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        static LEASE_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

        ATTEMPTS.store(0, Ordering::Release);
        LEASE_ATTEMPTS.store(0, Ordering::Release);
        POLL_CALLS.store(0, Ordering::Release);
        let poller = test_poller();
        let mut lease = None;
        let result = try_io_or_advance_with_lease(
            &poller,
            &mut lease,
            PollReason::Rx,
            || {
                LEASE_ATTEMPTS.fetch_add(1, Ordering::AcqRel);
                None
            },
            || {
                ATTEMPTS.fetch_add(1, Ordering::AcqRel);
                Ok(7)
            },
        );

        assert_eq!(result.unwrap(), 7);
        assert_eq!(ATTEMPTS.load(Ordering::Acquire), 1);
        assert_eq!(LEASE_ATTEMPTS.load(Ordering::Acquire), 0);
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 0);
        assert!(lease.is_none());
    }

    #[def_test(serial)]
    fn blocked_io_acquires_lease_before_retry() {
        static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        static LEASE_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

        ATTEMPTS.store(0, Ordering::Release);
        LEASE_ATTEMPTS.store(0, Ordering::Release);
        POLL_CALLS.store(0, Ordering::Release);
        let poller = test_poller();
        let mut lease = None;
        let result = try_io_or_advance_with_lease(
            &poller,
            &mut lease,
            PollReason::Rx,
            || {
                LEASE_ATTEMPTS.fetch_add(1, Ordering::AcqRel);
                poller
                    .poller
                    .acquire_lease_at_for_tests(u64::MAX)
                    .map(|inner| NetAdvanceLease {
                        owner: &poller,
                        inner,
                    })
            },
            || {
                let attempt = ATTEMPTS.fetch_add(1, Ordering::AcqRel);
                if attempt == 0 {
                    Err(KError::WouldBlock)
                } else {
                    Ok(7)
                }
            },
        );

        assert_eq!(result.unwrap(), 7);
        assert_eq!(ATTEMPTS.load(Ordering::Acquire), 2);
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 1);
        assert_eq!(LEASE_ATTEMPTS.load(Ordering::Acquire), 1);
        assert!(lease.is_some());
        lease.take();
    }
}
