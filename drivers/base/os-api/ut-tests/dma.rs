// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use crate::{DmaInfo, DmaWindow, ResError};

#[def_test]
fn streaming_guard_retains_release_owner_and_releases_once_after_move() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    let releases = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(());
    let weak_owner = Arc::downgrade(&owner);
    let observed = releases.clone();
    let hook: crate::ReleaseHook = Arc::new(move || {
        let _retained = &owner;
        observed.fetch_add(1, Ordering::Relaxed);
    });
    // SAFETY: this synthetic mapping is never submitted or dereferenced. Its
    // sole release hook retains the simulated provider and records cleanup.
    let mapping = unsafe { crate::StreamingMapping::new(0, 64, hook) };
    assert_eq!(mapping.bus_addr(), 0);
    assert_eq!(mapping.len(), 64);
    assert_eq!(mapping.is_empty(), false);
    let moved = mapping;
    assert_eq!(weak_owner.upgrade().is_some(), true);
    assert_eq!(releases.load(Ordering::Relaxed), 0);
    drop(moved);
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    assert_eq!(weak_owner.upgrade().is_none(), true);
}

#[def_test]
fn dma_window_checks_both_ends_of_the_allocation() {
    let view = DmaInfo {
        mask: u64::MAX,
        window: DmaWindow::new(0x8000, 0x4000, 0x1000).unwrap(),
        coherent: true,
    };
    assert_eq!(view.translate_range(0x8000, 0x1000), Some(0x4000));
    assert_eq!(view.translate_range(0x7fff, 2), None);
    assert_eq!(view.translate(0x8fff), Some(0x4fff));
    assert_eq!(view.translate_range(0x8fff, 2), None);
    assert_eq!(view.translate(0x9000), None);
    assert_eq!(view.translate_range(0x8000, 0), None);
    assert_eq!(view.translate_range(0x8001, u64::MAX), None);
}

#[def_test]
fn dma_mask_checks_translated_interval_not_only_base() {
    let view = DmaInfo {
        mask: 0xffff,
        window: DmaWindow::new(0x4000, 0x8000, 0x10000).unwrap(),
        coherent: false,
    };
    assert_eq!(view.translate_range(0x4000, 0x8000), Some(0x8000));
    assert_eq!(view.translate_range(0x4000, 0x8001), None);
    assert_eq!(view.translate(0xc000), None);
    let invalid_mask = DmaInfo {
        mask: 0b101,
        ..DmaInfo::DEFAULT
    };
    assert_eq!(invalid_mask.translate_range(1, 4), None);
}

#[def_test]
fn dma_window_rejects_invalid_firmware_extents() {
    assert_eq!(DmaWindow::new(0, 0, 0), Err(ResError::InvalidResource));
    assert_eq!(
        DmaWindow::new(u64::MAX, 0, 2),
        Err(ResError::InvalidResource)
    );
    assert_eq!(
        DmaWindow::new(0, u64::MAX, 2),
        Err(ResError::InvalidResource)
    );
}

#[def_test]
fn dma_translation_preserves_full_width_without_signed_offset() {
    let high_bus = DmaInfo {
        window: DmaWindow::new(0, u64::MAX - 15, 16).unwrap(),
        ..DmaInfo::DEFAULT
    };
    assert_eq!(high_bus.translate_range(0, 16), Some(u64::MAX - 15));
    assert_eq!(high_bus.translate(15), Some(u64::MAX));
    assert_eq!(high_bus.translate(16), None);
    let high_cpu = DmaInfo {
        window: DmaWindow::new(u64::MAX - 15, 0, 16).unwrap(),
        ..DmaInfo::DEFAULT
    };
    assert_eq!(high_cpu.translate_range(u64::MAX - 15, 16), Some(0));
    assert_eq!(high_cpu.translate_range(u64::MAX, 2), None);
    assert_eq!(DmaInfo::DEFAULT.translate(u64::MAX), Some(u64::MAX));
    assert_eq!(DmaInfo::DEFAULT.translate_range(u64::MAX, 2), None);
}
