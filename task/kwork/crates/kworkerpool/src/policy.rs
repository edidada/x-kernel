// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-pool lifecycle and accounting policy.

use ktime_types::TimeSpan;

/// Default delay before retrying worker creation after a creation failure.
pub const DEFAULT_CREATE_RETRY_DELAY: TimeSpan = TimeSpan::from_millis(10);

/// Default runtime after which a running execution stops consuming pool
/// concurrency.
pub const DEFAULT_CPU_INTENSIVE_THRESHOLD: TimeSpan = TimeSpan::from_millis(10);

/// Default idle time after which an extra worker may retire.
pub const DEFAULT_IDLE_RETIRE_AFTER: TimeSpan = TimeSpan::from_secs(30);

/// Runtime target used when the pool decides runnable work should execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolDispatchTarget {
    /// Wake a task-backed worker thread.
    WorkerThread,
    /// Raise a bottom-half style execution context.
    BottomHalf,
}

/// Per-instance worker-pool lifecycle and accounting policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPolicy {
    min_workers: usize,
    initial_workers: usize,
    max_workers: usize,
    idle_retire_after: Option<TimeSpan>,
    create_retry_delay: TimeSpan,
    cpu_intensive_threshold: TimeSpan,
    dispatch_target: WorkerPoolDispatchTarget,
    scalable: bool,
    idle_retire: bool,
}

/// Explicit inputs used to create one worker-pool policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPolicyConfig {
    /// Minimum number of installed workers retained by the pool.
    pub min_workers: usize,
    /// Number of workers expected during initial runtime bring-up.
    pub initial_workers: usize,
    /// Maximum number of runtime worker slots.
    pub max_workers: usize,
    /// Idle time after which extra idle workers may retire.
    pub idle_retire_after: Option<TimeSpan>,
    /// Backoff before retrying runtime worker creation after a failure.
    pub create_retry_delay: TimeSpan,
    /// Runtime after which an execution stops consuming bounded concurrency.
    pub cpu_intensive_threshold: TimeSpan,
    /// Runtime target used for dispatch actions.
    pub dispatch_target: WorkerPoolDispatchTarget,
    /// Whether the pool may grow beyond its initial worker count.
    pub scalable: bool,
    /// Whether the pool may retire idle workers above `min_workers`.
    pub idle_retire: bool,
}

impl WorkerPoolPolicy {
    /// Creates a fixed-size pool policy.
    ///
    /// A fixed-size pool installs `workers` runtime workers, wakes those
    /// workers directly, never requests creation of more workers, and never
    /// retires idle workers. This is the simplest policy for runtimes that
    /// already know their worker count.
    pub const fn fixed_threaded(workers: usize) -> Self {
        Self::new(WorkerPoolPolicyConfig {
            min_workers: workers,
            initial_workers: workers,
            max_workers: workers,
            idle_retire_after: None,
            create_retry_delay: TimeSpan::from_millis(0),
            cpu_intensive_threshold: DEFAULT_CPU_INTENSIVE_THRESHOLD,
            dispatch_target: WorkerPoolDispatchTarget::WorkerThread,
            scalable: false,
            idle_retire: false,
        })
    }

    /// Creates a scalable task-backed pool policy.
    ///
    /// The pool starts with `min_workers`, may grow up to `max_workers` while
    /// runnable work is not making progress, and may retire extra idle workers
    /// back down to `min_workers`. Creation retry, CPU-intensive accounting, and
    /// idle-retire timing use this crate's default values.
    pub const fn scalable_threaded(min_workers: usize, max_workers: usize) -> Self {
        Self::new(WorkerPoolPolicyConfig {
            min_workers,
            initial_workers: min_workers,
            max_workers,
            idle_retire_after: Some(DEFAULT_IDLE_RETIRE_AFTER),
            create_retry_delay: DEFAULT_CREATE_RETRY_DELAY,
            cpu_intensive_threshold: DEFAULT_CPU_INTENSIVE_THRESHOLD,
            dispatch_target: WorkerPoolDispatchTarget::WorkerThread,
            scalable: true,
            idle_retire: true,
        })
    }

    /// Creates a fixed-size bottom-half pool policy.
    ///
    /// A bottom-half pool does not wake task-backed workers. Dispatch returns a
    /// bottom-half style action and the product layer drives execution from
    /// that context.
    pub const fn bottom_half(workers: usize) -> Self {
        Self::new(WorkerPoolPolicyConfig {
            min_workers: workers,
            initial_workers: workers,
            max_workers: workers,
            idle_retire_after: None,
            create_retry_delay: TimeSpan::from_millis(0),
            cpu_intensive_threshold: DEFAULT_CPU_INTENSIVE_THRESHOLD,
            dispatch_target: WorkerPoolDispatchTarget::BottomHalf,
            scalable: false,
            idle_retire: false,
        })
    }

    /// Creates a policy with explicit fields.
    ///
    /// Worker counts are normalized into `min <= initial <= max`. Growth and
    /// idle-retire switches are also disabled when the normalized count range
    /// cannot make those policies observable.
    pub const fn new(config: WorkerPoolPolicyConfig) -> Self {
        let max_workers = config.max_workers;
        let min_workers = if config.min_workers > max_workers {
            max_workers
        } else {
            config.min_workers
        };
        let initial_workers = if config.initial_workers < min_workers {
            min_workers
        } else if config.initial_workers > max_workers {
            max_workers
        } else {
            config.initial_workers
        };
        let scalable = config.scalable && min_workers < max_workers;
        let idle_retire = scalable
            && config.idle_retire
            && min_workers < max_workers
            && config.idle_retire_after.is_some();
        let idle_retire_after = if idle_retire {
            config.idle_retire_after
        } else {
            None
        };

        Self {
            min_workers,
            initial_workers,
            max_workers,
            idle_retire_after,
            create_retry_delay: config.create_retry_delay,
            cpu_intensive_threshold: config.cpu_intensive_threshold,
            dispatch_target: config.dispatch_target,
            scalable,
            idle_retire,
        }
    }

    /// Minimum installed workers retained by this pool.
    pub const fn min_workers(self) -> usize {
        self.min_workers
    }

    /// Number of workers expected during initial pool bring-up.
    pub const fn initial_workers(self) -> usize {
        self.initial_workers
    }

    /// Maximum runtime worker slots.
    pub const fn max_workers(self) -> usize {
        self.max_workers
    }

    /// Idle duration after which an idle worker may retire.
    pub const fn idle_retire_after(self) -> Option<TimeSpan> {
        self.idle_retire_after
    }

    /// Retry delay after worker creation fails.
    pub const fn create_retry_delay(self) -> TimeSpan {
        self.create_retry_delay
    }

    /// Runtime after which a current execution stops counting toward pool
    /// concurrency.
    pub const fn cpu_intensive_threshold(self) -> TimeSpan {
        self.cpu_intensive_threshold
    }

    /// Returns the runtime target for dispatch actions.
    pub const fn dispatch_target(self) -> WorkerPoolDispatchTarget {
        self.dispatch_target
    }

    /// Returns whether dispatch wakes task-backed worker threads.
    pub const fn wakes_worker_threads(self) -> bool {
        matches!(self.dispatch_target, WorkerPoolDispatchTarget::WorkerThread)
    }

    /// Returns whether this pool may grow beyond its initial worker count.
    pub const fn scalable(self) -> bool {
        self.scalable
    }

    /// Returns whether idle workers may retire.
    pub const fn idle_retire(self) -> bool {
        self.idle_retire
    }
}
