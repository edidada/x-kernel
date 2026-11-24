// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-lifetime host adapter for the kdevice deferred-probe drain work.

use alloc::sync::Arc;

use kdevice::{DeferredExecutor, drain_deferred, install_deferred_executor};
use kworkng::{SubmitError, WorkHandle};

struct HostDeferredExecutor {
    work: WorkHandle<fn()>,
}

#[cfg(unittest)]
type TestWork = WorkHandle<fn()>;

#[cfg(unittest)]
kspin::static_lock! {
    static TEST_WORK: kspin::SpinNoIrq<Option<TestWork>> = kspin::SpinNoIrq::new(None);
}

#[cfg(unittest)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: cfg(unittest)-dependent helper is referenced only from \
              cfg(unittest) ut-tests"
)]
#[allow(
    dead_code,
    reason = "CG-024-lint-exceptions: referenced from cfg(unittest) ut-tests only when \
              deferred_probe is compiled"
)]
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: test barrier requires sleepable task context"
)]
/// Wait until every accepted drain callback has returned.
pub(crate) fn flush_for_tests() {
    let work = TEST_WORK.lock().clone();
    if let Some(work) = work {
        work.flush()
            .expect("deferred drain callback must be flushable from test task context");
    }
}

impl DeferredExecutor for HostDeferredExecutor {
    fn kick(&self) {
        match self.work.queue_default(None) {
            Ok(_) | Err(SubmitError::AlreadyQueued) => {}
            Err(error) => {
                // The default normal queue is permanent and this work is never
                // disabled/closed by the fixed-lifetime contract.
                fatal_submit(error);
            }
        }
    }
}

#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: the fixed-lifetime default queue cannot \
              reject the single drain work; rejection is an internal provider invariant failure"
)]
fn fatal_submit(error: SubmitError) -> ! {
    panic!("deferred drain work admission failed: {error:?}");
}

/// Install the one drain work before driver discovery starts.
///
/// Called by `init_drivers` after `kworkng::runtime::init` and the registry
/// initialization, so this context is sleepable and the normal workqueue is
/// already available.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: boot cannot start driver discovery without \
              the deferred executor; initialization runs after workqueue bring-up"
)]
pub(crate) fn init() {
    if kdevice::deferred_executor_installed() {
        return;
    }
    let work = WorkHandle::new(drain_deferred as fn())
        .expect("deferred drain work must be prepared during driver initialization");
    #[cfg(unittest)]
    {
        *TEST_WORK.lock() = Some(work.clone());
    }
    install_deferred_executor(Arc::new(HostDeferredExecutor { work }));
}
