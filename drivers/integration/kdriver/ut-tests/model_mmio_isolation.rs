// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opt-in two-device real MMIO integration coverage with responding RNG backends.
//!
//! Publication identity is inferred from which initial handle closes on unbind,
//! not from registry order or source metadata. Disable the entropy consumer and
//! both other lifecycle opt-ins; no other consumer or manager may race this case.
//! Reads/reset may block: the outer QEMU timeout bounds the experiment, not a
//! production timeout. This does not certify pending cancellation or IRQ tails.
//!
//! `drivers/devices/virtio/src/rng.rs` retains at most 256 readable bytes in its
//! single MAX_REQUEST_BYTES buffer; submit preserves that cache until consumed.
//! Each sampling phase therefore consumes 257 bytes to require fresh backend
//! progress rather than pass on leftovers from an earlier phase. Keep this
//! test-local target above that bound if the production buffer size changes.

use alloc::vec::Vec;

use driver_model::{DeviceRef, DeviceState, RuntimeError, global_domain};
use subsys_hwrng::{HWRNG, HwrngHandle};
use unittest::{assert, assert_eq, def_test};

const DRIVER_NAME: &str = "virtio-rng-mmio-model";
const SAMPLE_LEN: usize = 16;
const SAMPLE_TOTAL_BYTES: usize = 257;

fn publications() -> Vec<HwrngHandle> {
    HWRNG
        .devices()
        .into_iter()
        .filter(|handle| handle.driver_name() == Some(DRIVER_NAME))
        .collect()
}

fn has_state(device: &DeviceRef, state: DeviceState) -> bool {
    global_domain().snapshot(device).is_some_and(|snapshot| {
        snapshot.device.same_device(device)
            && snapshot.state == state
            && snapshot.driver
                == if state == DeviceState::Bound {
                    Some(DRIVER_NAME)
                } else {
                    None
                }
            && device.is_online()
    })
}

fn sample_is_valid(handle: &HwrngHandle) -> bool {
    let mut remaining = SAMPLE_TOTAL_BYTES;
    while remaining > 0 {
        let requested = remaining.min(SAMPLE_LEN);
        let mut buffer = [0xa5u8; SAMPLE_LEN + 8];
        let result = handle.read(&mut buffer[4..4 + requested]);
        match result {
            Ok(count) if count > 0 && count <= requested => {
                if buffer[..4] != [0xa5; 4] || !buffer[4 + count..].iter().all(|byte| *byte == 0xa5)
                {
                    return false;
                }
                remaining -= count;
            }
            _ => return false,
        }
    }
    true
}

fn stale_is_closed(handle: &HwrngHandle) -> bool {
    if handle.is_available() {
        return false;
    }
    let mut buffer = [0xa5u8; SAMPLE_LEN];
    handle.read(&mut buffer) == Err(kerrno::KError::BadState) && buffer == [0xa5; SAMPLE_LEN]
}

struct RestoreBinding {
    device: DeviceRef,
    is_armed: bool,
}

impl RestoreBinding {
    fn restore(&mut self) -> Result<(), RuntimeError> {
        global_domain().bind_device(&self.device)?;
        if !has_state(&self.device, DeviceState::Bound) {
            return Err(RuntimeError::BadState);
        }
        self.is_armed = false;
        Ok(())
    }
}

impl Drop for RestoreBinding {
    fn drop(&mut self) {
        if self.is_armed
            && let Err(error) = self.restore()
        {
            log::error!(
                "model MMIO isolation: restore {} failed: {error:?}; registration retained",
                self.device.path()
            );
        }
    }
}

/// Identify instances only after observing an unambiguous withdrawal.
fn withdrawn_pair(initial: &[HwrngHandle]) -> Option<(&HwrngHandle, &HwrngHandle)> {
    let [first, second] = initial else {
        return None;
    };
    let pair = match (first.is_available(), second.is_available()) {
        (false, true) => (first, second),
        (true, false) => (second, first),
        _ => return None,
    };
    let current = publications();
    match current.as_slice() {
        [survivor] if survivor.same_publication(pair.1) => Some(pair),
        _ => None,
    }
}

fn restored_sample(old: &HwrngHandle, peer: &HwrngHandle) -> bool {
    let current = publications();
    if current.len() != 2 || !current.iter().all(HwrngHandle::is_available) {
        return false;
    }
    let peers = current
        .iter()
        .filter(|handle| handle.same_publication(peer))
        .count();
    let mut replacements = current
        .iter()
        .filter(|handle| !handle.same_publication(peer));
    let Some(replacement) = replacements.next() else {
        return false;
    };
    if peers != 1 || replacements.next().is_some() || replacement.same_publication(old) {
        return false;
    }
    sample_is_valid(replacement) && sample_is_valid(peer)
}

fn isolation_round(device: &DeviceRef, peer_device: &DeviceRef) -> Result<(), &'static str> {
    let initial = publications();
    if initial.len() != 2
        || !initial.iter().all(HwrngHandle::is_available)
        || initial[0].same_publication(&initial[1])
        || !has_state(device, DeviceState::Bound)
        || !has_state(peer_device, DeviceState::Bound)
    {
        return Err("round requires two distinct active publications and devices");
    }
    let mut restore = RestoreBinding {
        device: device.clone(),
        is_armed: true,
    };
    let unbind = global_domain().unbind_device(device);
    let is_unclaimed = has_state(device, DeviceState::Unclaimed);
    let peer_active = has_state(peer_device, DeviceState::Bound);
    let pair = withdrawn_pair(&initial);
    let during_unbind = if unbind.is_ok() && is_unclaimed && peer_active {
        pair.is_some_and(|(old, peer)| stale_is_closed(old) && sample_is_valid(peer))
    } else {
        false
    };
    let rebound = restore.restore();
    drop(restore);
    let both_active =
        has_state(device, DeviceState::Bound) && has_state(peer_device, DeviceState::Bound);
    let after_rebind = if rebound.is_ok() && both_active {
        pair.is_some_and(|(old, peer)| stale_is_closed(old) && restored_sample(old, peer))
    } else {
        false
    };
    if unbind.is_err() || !is_unclaimed || !peer_active || !during_unbind {
        return Err("withdrawal did not isolate exactly one publication and preserve peer I/O");
    }
    if rebound.is_err() || !both_active || !after_rebind {
        return Err("restoration did not preserve cores/peer, replace publication and restore I/O");
    }
    Ok(())
}

fn have_distinct_mmio(first: &DeviceRef, second: &DeviceRef) -> bool {
    let Some(first) = first.descriptor().resources.mmio(0) else {
        return false;
    };
    let Some(second) = second.descriptor().resources.mmio(0) else {
        return false;
    };
    let Some(first_end) = first.base.checked_add(first.size) else {
        return false;
    };
    let Some(second_end) = second.base.checked_add(second.size) else {
        return false;
    };
    first.size > 0 && second.size > 0 && (first_end <= second.base || second_end <= first.base)
}

#[def_test(serial)]
fn real_model_mmio_rng_unbind_preserves_other_device() {
    if option_env!("XK_TEST_MODEL_MMIO_ISOLATION") != Some("1") {
        return unittest::TestResult::Ignored;
    }
    assert!(cfg!(target_arch = "aarch64"));
    assert!(kbuild_config::KFEAT_VIRTIO_BUS_MMIO);
    assert!(!kbuild_config::KFEAT_ENTROPY_VIRTIO_RNG);
    assert!(option_env!("XK_TEST_MODEL_MMIO_LIFECYCLE") != Some("1"));
    assert!(option_env!("XK_TEST_MODEL_MMIO_PENDING") != Some("1"));
    assert!(ktask::is_sleepable_context());
    let devices: Vec<_> = global_domain()
        .devices()
        .into_iter()
        .filter(|snapshot| snapshot.driver == Some(DRIVER_NAME))
        .map(|snapshot| snapshot.device)
        .collect();
    assert_eq!(devices.len(), 2);
    let first = &devices[0];
    let second = &devices[1];
    assert!(!first.same_device(second));
    assert!(first.path() != second.path());
    assert!(have_distinct_mmio(first, second));
    assert_eq!(isolation_round(first, second), Ok(()));
    assert_eq!(isolation_round(second, first), Ok(()));
}
