// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{
    DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_START, DMA_BUF_SYNC_WRITE,
    classify_sync,
};
use crate::object::CpuAccess;

/// The direction bits are separate bits, not an enum, so `READ|WRITE` is one
/// access in both directions and never a misread of either single direction.
#[def_test]
fn directions_are_distinct_bits() {
    assert_eq!(DMA_BUF_SYNC_RW, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE);
    assert_eq!(DMA_BUF_SYNC_START, 0);
    assert_eq!(DMA_BUF_SYNC_READ & DMA_BUF_SYNC_END, 0);
    assert_eq!(DMA_BUF_SYNC_WRITE & DMA_BUF_SYNC_END, 0);
}

/// A start request names what the CPU is about to do so the exporter can
/// invalidate before the read; an end request names what it did so the
/// exporter can clean before the device reads.
#[def_test]
fn phase_and_direction_classify_together() {
    let start_read = classify_sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)
        .expect("a start of read access is a valid request");
    assert_eq!(start_read.access, CpuAccess::Read);
    assert!(!start_read.is_end);

    let end_write = classify_sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE)
        .expect("an end of write access is a valid request");
    assert_eq!(end_write.access, CpuAccess::Write);
    assert!(end_write.is_end);

    let start_rw = classify_sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW)
        .expect("a bidirectional start is a valid request");
    assert_eq!(start_rw.access, CpuAccess::ReadWrite);
    assert!(!start_rw.is_end);

    let end_rw = classify_sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW).expect("a bidirectional end");
    assert_eq!(end_rw.access, CpuAccess::ReadWrite);
    assert!(end_rw.is_end);
}

/// A request without a direction, and any bit outside the ABI mask, is
/// `EINVAL` — there is no "sync everything" spelling and no legacy bit to
/// honour.
#[def_test]
fn invalid_requests_are_refused() {
    assert_eq!(classify_sync(0), None);
    assert_eq!(classify_sync(DMA_BUF_SYNC_END), None);
    // `DMA_BUF_SYNC_NOWAIT` is an Android-era bit outside the mask.
    assert_eq!(classify_sync(DMA_BUF_SYNC_READ | (1 << 4)), None);
    assert_eq!(classify_sync(u64::MAX), None);
}

/// The CPU-side access helpers answer from the CPU's point of view: a read
/// access consumes what a device wrote, a write access publishes to it.
#[def_test]
fn access_direction_helpers_follow_the_cpu_side() {
    assert!(CpuAccess::Read.reads_memory());
    assert!(!CpuAccess::Read.writes_memory());
    assert!(!CpuAccess::Write.reads_memory());
    assert!(CpuAccess::Write.writes_memory());
    assert!(CpuAccess::ReadWrite.reads_memory());
    assert!(CpuAccess::ReadWrite.writes_memory());
}