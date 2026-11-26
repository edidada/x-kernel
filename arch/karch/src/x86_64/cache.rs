// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for x86_64.
//!
//! x86_64 maintains instruction cache coherency in hardware, so all icache
//! flush operations are no-ops.

use memaddr::VirtAddr;

/// Local icache flush — no-op on x86_64.
#[inline]
pub fn flush_icache_all_local() {}

/// What a remote CPU must execute when receiving an icache-flush IPI.
///
/// x86_64 maintains icache coherency in hardware, so this is a no-op.
#[inline]
pub fn flush_icache_remote() {}

/// Flushes the entire instruction cache (no-op on x86_64).
#[inline]
pub fn flush_icache_all() {}

/// Flushes the instruction cache for the given virtual address range (no-op on x86_64).
#[inline]
pub fn flush_icache_range(_start: VirtAddr, _size: usize) {}

/// Orders prior CPU stores before a device reads the same memory via DMA.
///
/// x86_64 is cache-coherent for device DMA and maintains strong store
/// ordering, so no barrier is required. The explicit no-op keeps driver call
/// sites portable across architectures — the LoongArch64 implementation
/// executes `dbar 0`.
#[inline]
pub fn dma_read_barrier() {}

/// Cleans `[start, start + size)` so a non-coherent device observes this CPU's
/// writes.
///
/// x86_64 maintains cache coherency with DMA in hardware, so there is
/// nothing to clean. The explicit no-op keeps device-buffer call sites
/// portable across architectures, exactly like [`dma_read_barrier`].
#[inline]
pub fn dma_clean_range(_start: VirtAddr, _size: usize) {}

/// Invalidates `[start, start + size)` so the CPU observes a non-coherent
/// device's writes.
///
/// Same reasoning as [`dma_clean_range`].
#[inline]
pub fn dma_invalidate_range(_start: VirtAddr, _size: usize) {}
