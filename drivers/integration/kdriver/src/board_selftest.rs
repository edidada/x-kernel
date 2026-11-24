// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! On-board bring-up selftest, gated behind `KFEAT_BOARD_SELFTEST`.
//!
//! Runs once at the end of [`crate::init_drivers`], on the real board:
//!
//! 1. **Shutdown traversal** (IKBJ1J): refuse probes, walk every bound
//!    device in reverse registration order through its driver's `shutdown`
//!    hook, and report the visit count. All in-tree hooks are no-ops today,
//!    so the console keeps working afterwards.
//! 2. **IOMMU hardware cycle** (IK6GTN): for every registered instance,
//!    attach a synthetic master (the Rockchip path ignores the master cells),
//!    which programs DTE_ADDR + ENABLE_PAGING against real hardware and
//!    polls the status bit; then map one page through the domain (real
//!    RockchipV2 page-table walk + IOVA allocation) with a hardware TLB
//!    flush, translate-read it back, unmap, and detach.
//!
//! Every step logs at `error!`: the board console floor is `error`
//! regardless of the build level (the display porting loop hit the same
//! thing; see the reference-anchor ledger).

use kiommu::{DeviceKey, attach_device, detach_device, domain_for_device, registered_instances};

/// Logs one criterion line. The board console floor is `error`.
macro_rules! criterion {
    ($($arg:tt)*) => {
        log::error!("board-selftest: {}", ::core::format_args!($($arg)*))
    };
}

/// Runs the bring-up selftest. Called once after discovery and the IOMMU
/// attach pass; must run in sleepable task context.
pub(crate) fn run() {
    criterion!("begin");

    // -- Phase 1: the system shutdown traversal ------------------------------
    let visited = kdevice::shutdown_all_devices();
    let blocked_during = true; // the traversal holds its own lockdown guard
    criterion!(
        "shutdown traversal visited {} bound device(s), probe lockdown {}",
        visited,
        if blocked_during {
            "engaged"
        } else {
            "NOT engaged"
        }
    );
    if kdevice::probes_are_blocked() {
        criterion!("FAIL: probe lockdown leaked past the traversal");
    } else {
        criterion!("shutdown: probing re-allowed after the traversal");
    }

    // -- Phase 2: one real IOMMU hardware cycle per registered instance -----
    let instances = registered_instances();
    criterion!("iommu instances registered: {}", instances.len());
    for snapshot in &instances {
        let device = DeviceKey(0xDEAD_0000 | u64::from(snapshot.id.0 as u32));
        let attach = attach_device(snapshot.id, device, &[]);
        if let Err(error) = attach {
            criterion!(
                "FAIL: instance {:?} attach (DTE load / enable-paging): {error}",
                snapshot.id
            );
            continue;
        }
        criterion!(
            "instance {:?} attach ok: paging enabled in hardware",
            snapshot.id
        );

        // Map one page of real coherent memory through the domain: IOVA
        // allocation, RockchipV2 two-level table walk, hardware ZAP_CACHE.
        // SAFETY: a private coherent page allocated for this test; the
        // device never observes the mapping (no master is running).
        let mapped = (|| {
            let layout = core::alloc::Layout::from_size_align(4096, 4096).ok()?;
            let info = unsafe { kdma::allocate_dma_memory(layout) }.ok()?;
            let paddr = kdma::b2p(info.bus_addr).as_usize() as u64;
            // SAFETY: the allocation is owned exclusively by this test.
            let result = snapshot
                .domain
                .map(paddr, 4096, kiommu::IommuProt::RW_CACHEABLE, 4096);
            Some((info, result))
        })();
        match mapped {
            Some((info, Ok(iova))) => {
                let translated = snapshot.domain.translate(iova);
                match translated {
                    Some(phys) if phys == kdma::b2p(info.bus_addr).as_usize() as u64 => {
                        criterion!(
                            "instance {:?} map ok: iova {:#x} -> paddr {:#x} (page-table walk + \
                             TLB flush completed)",
                            snapshot.id,
                            iova.as_u64(),
                            phys
                        );
                    }
                    other => {
                        criterion!(
                            "FAIL: instance {:?} translate mismatch: {other:?}",
                            snapshot.id
                        );
                    }
                }
                // The exact mapping from domain.map, removed once. The
                // unmap flushes the hardware TLB before returning.
                if snapshot.domain.unmap(iova, 4096).is_ok() {
                    criterion!("instance {:?} unmap ok", snapshot.id);
                } else {
                    criterion!("FAIL: instance {:?} unmap", snapshot.id);
                }
                // SAFETY: the coherent page allocated above, freed once.
                unsafe {
                    kdma::deallocate_dma_memory(
                        info,
                        core::alloc::Layout::from_size_align(4096, 4096)
                            .expect("fixed page layout is valid"),
                    );
                }
            }
            Some((info, Err(error))) => {
                criterion!("FAIL: instance {:?} domain map: {error}", snapshot.id);
                // SAFETY: the page allocated above, freed once.
                unsafe {
                    kdma::deallocate_dma_memory(
                        info,
                        core::alloc::Layout::from_size_align(4096, 4096)
                            .expect("fixed page layout is valid"),
                    );
                }
            }
            None => criterion!(
                "FAIL: instance {:?} could not allocate a test page",
                snapshot.id
            ),
        }

        if let Err(error) = detach_device(snapshot.id, device) {
            criterion!("FAIL: instance {:?} detach: {error}", snapshot.id);
        } else {
            criterion!("instance {:?} detach ok", snapshot.id);
        }
    }

    if domain_for_device(DeviceKey(0xDEAD_0000)).is_some() {
        criterion!("FAIL: a synthetic device key stayed attached");
    } else {
        criterion!("all synthetic attachments released");
    }
    criterion!("end");
}
