// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use crate::{CoherentMem, DevIrq, Virq};

#[def_test]
fn irq_and_dma_guards_release_only_their_acquisition() {
    let releases = Arc::new(AtomicUsize::new(0));
    let irq_release = releases.clone();
    let dma_release = releases.clone();
    let irq = DevIrq::new(
        Virq::new(41),
        Some(Arc::new(move || {
            irq_release.fetch_add(1, Ordering::Relaxed);
        })),
    );
    let dma = CoherentMem::new(
        0,
        0x4000,
        64,
        Some(Arc::new(move || {
            dma_release.fetch_add(10, Ordering::Relaxed);
        })),
    );
    assert_eq!(irq.virq(), Virq::new(41));
    assert_eq!(dma.bus_addr(), 0x4000);
    assert_eq!(dma.len(), 64);
    assert_eq!(releases.load(Ordering::Relaxed), 0);
    drop(irq);
    assert_eq!(releases.load(Ordering::Relaxed), 1);
    drop(dma);
    assert_eq!(releases.load(Ordering::Relaxed), 11);
}
