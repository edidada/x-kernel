// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::TimeSpan;

use crate::BuiltinPoolKind;

/// Per-instance worker-pool lifecycle and accounting policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPolicy {
    min_workers: usize,
    initial_workers: usize,
    max_workers: usize,
    idle_retire_after: Option<TimeSpan>,
    create_retry_delay: TimeSpan,
    cpu_intensive_threshold: TimeSpan,
    dynamic_create: bool,
    idle_retire: bool,
}

/// Explicit inputs used to create one worker-pool policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPolicyConfig {
    pub min_workers: usize,
    pub initial_workers: usize,
    pub max_workers: usize,
    pub idle_retire_after: Option<TimeSpan>,
    pub create_retry_delay: TimeSpan,
    pub cpu_intensive_threshold: TimeSpan,
    pub dynamic_create: bool,
    pub idle_retire: bool,
}

impl WorkerPoolPolicy {
    /// Creates a policy with explicit fields.
    pub const fn new(config: WorkerPoolPolicyConfig) -> Self {
        Self {
            min_workers: config.min_workers,
            initial_workers: config.initial_workers,
            max_workers: config.max_workers,
            idle_retire_after: config.idle_retire_after,
            create_retry_delay: config.create_retry_delay,
            cpu_intensive_threshold: config.cpu_intensive_threshold,
            dynamic_create: config.dynamic_create,
            idle_retire: config.idle_retire,
        }
    }

    /// Default policy template for a built-in pool kind.
    pub const fn for_kind(kind: BuiltinPoolKind) -> Self {
        match kind {
            BuiltinPoolKind::Normal => Self::new(WorkerPoolPolicyConfig {
                min_workers: 1,
                initial_workers: 1,
                max_workers: 4,
                idle_retire_after: Some(TimeSpan::from_secs(30)),
                create_retry_delay: TimeSpan::from_millis(10),
                cpu_intensive_threshold: TimeSpan::from_millis(10),
                dynamic_create: true,
                idle_retire: true,
            }),
            BuiltinPoolKind::Bh => Self::new(WorkerPoolPolicyConfig {
                min_workers: 1,
                initial_workers: 1,
                max_workers: 1,
                idle_retire_after: None,
                create_retry_delay: TimeSpan::from_millis(0),
                cpu_intensive_threshold: TimeSpan::from_millis(10),
                dynamic_create: false,
                idle_retire: false,
            }),
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

    /// Maximum provider worker slots.
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

    /// Returns whether this pool may dynamically create provider workers.
    pub const fn dynamic_create(self) -> bool {
        self.dynamic_create
    }

    /// Returns whether idle workers may retire.
    pub const fn idle_retire(self) -> bool {
        self.idle_retire
    }
}
