// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Removal entries: per-class unpublish + withdrawal guards + the
//! safety-net owner drain.

use super::*;

/// Safety-net drain: withdraws the detached publication of EVERY
/// registered class for the owning device id. NOT the production removal
/// path anymore — each driver `Data` owns its publications via
/// withdrawal guards (`ClassWithdraw`) and unpublishes at its own drop.
/// This remains for diagnostics (leaked-publicion audits) and as the
/// fallback for a driver that forgets its guard; the per-class entries
/// self-register below so new classes drain without editing callers.
pub fn remove_all_detached(owner: u64) {
    // Find each class publication for this owner, then run the FULL removal
    // path (removal callbacks, devnode withdraw, class unpublish) via the
    // per-class unpublish_* entries — the same code the family activation
    // close() paths use. This guarantees no cleanup step is bypassed and
    // both removal entry points (composition-root notifier and activation
    // close) are idempotent with each other.
    #[cfg(feature = "hwrng")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = hwrng_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(hwrng_class_registry, id);
    }
    #[cfg(feature = "char")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = char_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(char_class_registry, id);
    }
    #[cfg(feature = "net")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = net_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(net_class_registry, id);
    }
    #[cfg(feature = "input")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = input_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(input_class_registry, id);
    }
    #[cfg(feature = "display")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = display_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(display_class_registry, id);
    }
    #[cfg(feature = "vsock")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = vsock_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(vsock_class_registry, id);
    }
    #[cfg(feature = "virtio-9p")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = virtio_9p_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(virtio_9p_class_registry, id);
    }
    // Block publications carry the gendisk lifecycle in
    // ClassRuntimeMetadata::unpublish_class (del_gendisk); the full
    // class_remove path triggers it.
    #[cfg(feature = "block")]
    loop {
        // Bind via `let`-else: the registry lock guard is a temporary of the
        // scrutinee and would otherwise live through the loop body (the
        // `while let` temporary-lifetime rule) — class_remove re-locks the
        // same registry and would self-deadlock on the non-reentrant spin.
        let Some(id) = block_class_registry().lock().find_detached(owner) else {
            break;
        };
        class_remove(block_class_registry, id);
    }
}

#[cfg(feature = "virtio-9p")]
pub fn unpublish_virtio_9p(id: ClassDeviceId) {
    class_remove(virtio_9p_class_registry, id);
}

#[cfg(feature = "virtio-9p")]
pub fn virtio_9p_withdraw(device: &ClassDevice<Virtio9pDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_virtio_9p, device.id())
}

#[cfg(feature = "block")]
pub fn unpublish_block(device_number: devnode::DeviceNumber) {
    subsys_block::del_gendisk(device_number);
}

#[cfg(feature = "block")]
pub fn block_withdraw(device: &ClassDevice<BlockDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(|id| class_remove(block_class_registry, id), device.id())
}

#[cfg(feature = "hwrng")]
pub fn unpublish_hwrng(id: ClassDeviceId) {
    class_remove(hwrng_class_registry, id);
}

#[cfg(feature = "hwrng")]
pub fn hwrng_withdraw(device: &ClassDevice<HwrngDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_hwrng, device.id())
}

#[cfg(feature = "char")]
pub fn unpublish_char(id: ClassDeviceId) {
    class_remove(char_class_registry, id);
}

#[cfg(feature = "char")]
pub fn char_withdraw(device: &ClassDevice<CharDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_char, device.id())
}

#[cfg(feature = "net")]
pub fn unpublish_net(id: ClassDeviceId) {
    class_remove(net_class_registry, id);
}

#[cfg(feature = "net")]
pub fn net_withdraw(device: &ClassDevice<NetDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_net, device.id())
}

#[cfg(feature = "display")]
pub fn unpublish_display(id: ClassDeviceId) {
    class_remove(display_class_registry, id);
}

#[cfg(feature = "display")]
pub fn display_withdraw(device: &ClassDevice<DisplayDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_display, device.id())
}

#[cfg(feature = "input")]
pub fn unpublish_input(id: ClassDeviceId) {
    class_remove(input_class_registry, id);
}

#[cfg(feature = "input")]
pub fn input_withdraw(device: &ClassDevice<InputDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_input, device.id())
}

#[cfg(feature = "vsock")]
pub fn unpublish_vsock(id: ClassDeviceId) {
    class_remove(vsock_class_registry, id);
}

#[cfg(feature = "vsock")]
pub fn vsock_withdraw(device: &ClassDevice<VsockDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_vsock, device.id())
}
