// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-process `AT_RANDOM` bytes for the initial user stack.

use core::sync::atomic::{AtomicU64, Ordering};

use khal::time::now_ticks;
use rand_chacha::{
    ChaCha20Rng,
    rand_core::{RngCore, SeedableRng},
};

static AT_RANDOM_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fill `buf` with unpredictable bytes for the `AT_RANDOM` auxiliary vector.
///
/// Each exec image gets fresh output so stack canaries derived from `AT_RANDOM`
/// are not shared across unrelated processes.
pub fn fill_at_random(buf: &mut [u8; 16]) {
    let seq = AT_RANDOM_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&now_ticks().to_le_bytes());
    seed[8..16].copy_from_slice(&seq.to_le_bytes());
    let stack_hint = buf.as_ptr() as u64;
    seed[16..24].copy_from_slice(&stack_hint.to_le_bytes());
    let mut rng = ChaCha20Rng::from_seed(seed);
    rng.fill_bytes(buf);
}
