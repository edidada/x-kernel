// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel provider for RNG read-completion waiting.

use alloc::sync::Arc;

use kcompletion::WaitQueue;
use kerrno::{KError, KResult};
use ktask::completion::CompletionWait;
use subsys_hwrng::completion::{RngCompletion, RngDrain};

/// Construct a dedicated reliable drain queue before publishing an RNG.
///
/// Requires sleepable context. Arc allocation follows the kernel OOM policy;
/// unregister prepares only an existing task reference and a stack node.
pub fn new_rng_drain() -> KResult<Arc<dyn RngDrain>> {
    if !ktask::is_sleepable_context() {
        return Err(KError::InvalidInput);
    }
    Ok(Arc::new(HostRngDrain {
        waiters: WaitQueue::new(),
    }))
}

struct HostRngDrain {
    waiters: WaitQueue,
}

impl RngDrain for HostRngDrain {
    fn wait_until(&self, drained: &mut dyn FnMut() -> bool) -> KResult {
        let task_wait = CompletionWait::prepare().map_err(|_| KError::InvalidInput)?;
        task_wait.wait_until(&self.waiters, drained);
        Ok(())
    }

    fn notify(&self) {
        self.waiters.notify_all();
    }
}

/// Creates a shared completion capability for injection into one RNG driver.
///
/// Construct in task context. Allocation follows the kernel's Arc OOM policy.
/// The capability owns the queue; waiting stacks own scoped nodes. It owns
/// neither request state nor DMA storage.
pub fn new_rng_completion() -> Arc<dyn RngCompletion> {
    Arc::new(HostRngCompletion {
        waiters: WaitQueue::new(),
    })
}

struct HostRngCompletion {
    waiters: WaitQueue,
}

impl RngCompletion for HostRngCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
        let task_wait = CompletionWait::prepare().map_err(|_| KError::InvalidInput)?;
        task_wait.wait_until(&self.waiters, ready);
        Ok(())
    }

    fn notify(&self) {
        self.waiters.notify_all();
    }
}

unittest::path_mod_test!(
    rng_completion_unittest,
    "../ut-tests/rng_completion_unittest.rs"
);

unittest::path_mod_test!(rng_drain_unittest, "../ut-tests/rng_drain_unittest.rs");
