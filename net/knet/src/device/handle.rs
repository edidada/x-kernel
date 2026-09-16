// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device ownership and mutexes beneath the read-mostly interface inventory.
use alloc::{boxed::Box, string::String, sync::Arc};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use kerrno::{KResult, LinuxError};
use kpoll::{PollContext, PollRegisterError};
use ksync::{Mutex, MutexGuard};
use ktime_types::MonotonicInstant;

use super::{
    LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate, NeighborUpdatePolicy, NetDevice,
};
use crate::{
    buf::PacketBuf,
    ip::{IpAddress, Ipv4Cidr},
};

struct DeviceShared {
    device: Mutex<Box<dyn NetDevice>>,
    device_id: Option<kdevice::DeviceId>,
    link_kind: LinkKind,
    /// Published by link config (`Release`); route snapshots load `Acquire`.
    mtu: AtomicUsize,
}

/// Shared identity for a live `NetDevice`.
///
/// Packet I/O borrows this handle while holding the inventory read lock, which
/// keeps source addresses and interface indices valid through submission.
/// Identity, link kind, and MTU are cached so route resolution does not take
/// the device mutex. Lock order is inventory, then this mutex.
#[derive(Clone)]
pub(crate) struct DeviceHandle {
    inner: Arc<DeviceShared>,
}

impl DeviceHandle {
    pub(crate) fn new(device: Box<dyn NetDevice>) -> Self {
        let device_id = device.device_id();
        let link_kind = device.link_kind();
        let mtu = device.mtu();
        Self {
            inner: Arc::new(DeviceShared {
                device: Mutex::new(device),
                device_id,
                link_kind,
                mtu: AtomicUsize::new(mtu),
            }),
        }
    }

    pub(crate) fn with<R>(&self, use_fn: impl FnOnce(&dyn NetDevice) -> R) -> R {
        use_fn(&**self.inner.device.lock())
    }

    pub(crate) fn with_mut<R>(&self, use_fn: impl FnOnce(&mut dyn NetDevice) -> R) -> R {
        use_fn(&mut **self.inner.device.lock())
    }

    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, Box<dyn NetDevice>>> {
        self.inner.device.try_lock()
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Box<dyn NetDevice>> {
        self.inner.device.lock()
    }

    pub(crate) fn name(&self) -> String {
        self.with(|device| String::from(device.name()))
    }

    pub(crate) fn link_kind(&self) -> LinkKind {
        self.inner.link_kind
    }

    pub(crate) fn mtu(&self) -> usize {
        self.inner.mtu.load(Ordering::Acquire)
    }

    pub(crate) fn is_link_up(&self) -> bool {
        self.with(|device| device.is_link_up())
    }

    pub(crate) fn device_id(&self) -> Option<kdevice::DeviceId> {
        self.inner.device_id
    }

    pub(crate) fn has_rx_work(&self) -> bool {
        // A busy device must keep the poller scheduled without making its sole
        // executor wait behind a foreground sender or configuration operation.
        self.try_lock().is_none_or(|device| device.has_rx_work())
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dst: IpAddress) -> bool {
        self.with(|device| device.has_neighbor(dst))
    }

    pub(crate) fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        self.with(|device| device.link_snapshot(ifindex))
    }

    pub(crate) fn link_send_snapshot(&self) -> LinkSendSnapshot {
        self.with(|device| device.link_send_snapshot())
    }

    pub(crate) fn register_rx_waker(
        &self,
        source_waker: &Waker,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        self.with(|device| device.register_rx_waker(source_waker, context))
    }

    pub(crate) fn set_ipv4_addrs(&self, assigned_addrs: &[Ipv4Cidr], local_addrs: &[Ipv4Cidr]) {
        self.with_mut(|device| device.set_ipv4_addrs(assigned_addrs, local_addrs));
    }

    pub(crate) fn remove_pending_ipv4_source(&self, addr: crate::ip::Ipv4Address) {
        self.with_mut(|device| device.remove_pending_ipv4_source(addr));
    }

    pub(crate) fn apply_neighbor_update(
        &self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        let mut device = self.lock();
        let exists = device.has_neighbor(update.dst);
        if exists && !policy.can_replace {
            return Err(LinuxError::EEXIST);
        }
        if !exists && !policy.can_create {
            return Err(LinuxError::ENOENT);
        }
        device.apply_neighbor_update(update)
    }

    pub(crate) fn apply_link_config(
        &self,
        name: Option<String>,
        mtu: Option<usize>,
        is_up: Option<bool>,
    ) -> Result<(), LinuxError> {
        let mut device = self.inner.device.lock();
        if let Some(mtu) = mtu {
            device.set_mtu(mtu)?;
            self.inner.mtu.store(device.mtu(), Ordering::Release);
        }
        if let Some(name) = name {
            device.set_name(name);
        }
        if let Some(is_up) = is_up {
            device.set_link_up(is_up);
        }
        Ok(())
    }

    pub(crate) fn send_ip_packet(
        &self,
        ifindex: i32,
        next_hop: IpAddress,
        source_addr: IpAddress,
        packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        self.with_mut(|device| {
            device.send_ip_packet(ifindex, next_hop, source_addr, packet, timestamp)
        })
    }

    pub(crate) fn send_link_frame(&self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.with_mut(|device| device.send_link_frame(ifindex, frame))
    }
}

impl PartialEq for DeviceHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for DeviceHandle {}

impl core::fmt::Debug for DeviceHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeviceHandle")
            .field("ptr", &Arc::as_ptr(&self.inner))
            .finish()
    }
}
