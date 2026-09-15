// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interface inventory, link configuration, and neighbor updates.

use alloc::{boxed::Box, string::String, vec::Vec};

use kerrno::{KError, KResult, LinuxError};

use super::{
    LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate,
    NeighborUpdatePolicy, NetDevice,
};
use crate::{
    consts::STANDARD_MTU,
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    netlink::RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE,
};

/// Owns live devices and their aggregate MTU under the service network transaction lock.
///
/// NetworkState owns the inventory; NetworkConfig coordinates route and stack projections.
pub(crate) struct InterfaceSet {
    devices: Vec<Box<dyn NetDevice>>,
    ipv4_addrs: Vec<Ipv4AddrEntry>,
    effective_mtu: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4AddrEntry {
    pub dev: usize,
    pub addr: Ipv4Cidr,
    pub scope: u8,
    /// Linux `ifa_broadcast` override. `None` means derive from the CIDR.
    pub broadcast: Option<Ipv4Address>,
}

impl Ipv4AddrEntry {
    fn effective_broadcast(self) -> Option<Ipv4Address> {
        self.broadcast.or_else(|| self.addr.broadcast())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Ipv4AddrSnapshot {
    pub entry: Ipv4AddrEntry,
    pub label: String,
}

impl InterfaceSet {
    /// Borrows the live interfaces in their current slot order.
    pub(crate) fn devices(&self) -> &[Box<dyn NetDevice>] {
        &self.devices
    }

    /// Borrows device backends for packet I/O without resizing the inventory.
    pub(crate) fn devices_mut(&mut self) -> &mut [Box<dyn NetDevice>] {
        &mut self.devices
    }

    pub(crate) fn new() -> Self {
        Self {
            devices: Vec::new(),
            ipv4_addrs: Vec::new(),
            effective_mtu: STANDARD_MTU,
        }
    }

    /// Returns the MTU advertised through the smoltcp device capabilities.
    pub fn effective_mtu(&self) -> usize {
        self.effective_mtu
    }

    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.devices
            .iter()
            .enumerate()
            .map(|(dev, device)| device.link_snapshot(dev as i32 + 1))
            .collect()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        Self::device_index(ifindex).is_some_and(|dev_index| self.devices.get(dev_index).is_some())
    }

    pub fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.devices.iter().position(|device| device.name() == name)
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_send_snapshot())
    }

    /// Applies a validated link update.
    ///
    /// Returns `Ok(true)` when the effective device MTU changed and the
    /// smoltcp interface must be rebuilt to refresh its cached capabilities.
    pub fn update_device_link(
        &mut self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<bool, LinuxError> {
        let dev = Self::device_index(ifindex).ok_or(LinuxError::ENODEV)?;
        let device = self.devices.get(dev).ok_or(LinuxError::ENODEV)?;
        if let Some(name) = update.name.as_deref() {
            crate::device::validate_interface_name(name)?;
            if self
                .devices
                .iter()
                .enumerate()
                .any(|(other_dev, device)| other_dev != dev && device.name() == name)
            {
                return Err(LinuxError::EEXIST);
            }
        }
        if let Some(mtu) = update.mtu {
            device.link_kind().validate_mtu(mtu)?;
        }

        let is_mtu_changed = update.mtu.is_some_and(|mtu| mtu != device.mtu());
        let effective_mtu = self.effective_mtu;

        let LinkConfigUpdate { name, mtu, is_up } = update;
        let device = &mut self.devices[dev];
        if let Some(mtu) = mtu {
            device.set_mtu(mtu)?;
        }
        if let Some(name) = name {
            device.set_name(name);
        }
        if let Some(is_up) = is_up {
            device.set_link_up(is_up);
        }
        if is_mtu_changed {
            self.recompute_effective_mtu();
        }
        Ok(self.effective_mtu != effective_mtu)
    }

    fn device_index(ifindex: i32) -> Option<usize> {
        (ifindex > 0).then_some((ifindex - 1) as usize)
    }

    pub fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        let device = self.devices.get_mut(update.dev).ok_or(LinuxError::ENODEV)?;
        let exists = device.has_neighbor(update.dst);
        if exists && !policy.can_replace {
            return Err(LinuxError::EEXIST);
        }
        if !exists && !policy.can_create {
            return Err(LinuxError::ENOENT);
        }
        device.apply_neighbor_update(update)
    }

    #[cfg(unittest)]
    pub fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.devices
            .get(dev)
            .is_some_and(|device| device.has_neighbor(dst))
    }

    fn recompute_effective_mtu(&mut self) {
        self.effective_mtu = self
            .devices
            .iter()
            .map(|device| device.mtu().min(u16::MAX as usize))
            .min()
            .unwrap_or(STANDARD_MTU);
    }

    pub fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        let dev_index = Self::device_index(ifindex).ok_or(KError::InvalidInput)?;
        let dev = self
            .devices
            .get_mut(dev_index)
            .ok_or(KError::from(LinuxError::ENODEV))?;
        dev.send_link_frame(ifindex, frame)
    }

    pub fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(device);
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        self.devices.len() - 1
    }

    pub(crate) fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> Option<usize> {
        let pos = self
            .devices
            .iter()
            .position(|device| device.device_id() == Some(id))?;
        let removed_sources: Vec<_> = self
            .ipv4_addrs
            .iter()
            .filter_map(|entry| (entry.dev == pos).then_some(entry.addr.address()))
            .collect();
        self.devices.remove(pos);
        self.ipv4_addrs.retain(|entry| entry.dev != pos);
        for entry in &mut self.ipv4_addrs {
            if entry.dev > pos {
                entry.dev -= 1;
            }
        }
        for source in removed_sources {
            if !self.has_ipv4_address(source) {
                for device in &mut self.devices {
                    device.remove_pending_ipv4_source(source);
                }
            }
        }
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        Some(pos)
    }

    pub fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.ipv4_addrs.iter().map(|entry| entry.addr).collect()
    }

    /// Returns whether `address` is an IPv4 limited or directed broadcast.
    pub(crate) fn ipv4_dest_requires_broadcast(&self, address: &IpAddress) -> bool {
        let IpAddress::Ipv4(address) = *address else {
            return false;
        };
        address.is_broadcast()
            || self
                .ipv4_addrs
                .iter()
                .any(|entry| entry.addr.is_directed_broadcast(address))
    }

    /// Returns the primary IPv4 address of the device named `name`.
    ///
    /// Device resolution and address lookup happen under one interface-set borrow so
    /// a concurrent device removal cannot renumber slots between the two
    /// steps, matching Linux `devinet_ioctl` holding `rtnl_lock`.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` for an unknown device and `EADDRNOTAVAIL` when the
    /// device has no IPv4 address.
    pub(crate) fn interface_ipv4_addr(&self, name: &str) -> Result<Ipv4Cidr, LinuxError> {
        let dev = self.device_index_by_name(name).ok_or(LinuxError::ENODEV)?;
        self.ipv4_addrs
            .iter()
            .find(|entry| entry.dev == dev)
            .map(|entry| entry.addr)
            .ok_or(LinuxError::EADDRNOTAVAIL)
    }

    pub(crate) fn first_ipv4_addr_for_device(&self, ifindex: u32) -> Option<Ipv4Cidr> {
        ifindex
            .checked_sub(1)
            .and_then(|dev| {
                self.ipv4_addrs
                    .iter()
                    .find(|entry| entry.dev == dev as usize)
            })
            .map(|entry| entry.addr)
    }

    pub fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        &self.ipv4_addrs
    }

    pub fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.ipv4_addrs
            .iter()
            .filter_map(|entry| {
                self.devices.get(entry.dev).map(|device| Ipv4AddrSnapshot {
                    entry: *entry,
                    label: String::from(device.name()),
                })
            })
            .collect()
    }

    pub fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if entry.dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if entry.addr.prefix_len() > 32 || !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        if self
            .ipv4_addrs
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            return Ok(());
        }
        if self.ipv4_addrs.len() >= smoltcp::config::IFACE_MAX_ADDR_COUNT {
            return Err(LinuxError::ENOBUFS);
        }

        self.ipv4_addrs.push(entry);
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    /// Updates the primary IPv4 address on `dev` in its owning address table.
    ///
    /// Other addresses on the device remain unchanged. Updating an existing
    /// address resets its scope and broadcast fields like Linux
    /// `SIOCSIFADDR`; setting the same local address is a no-op.
    pub fn set_primary_ipv4_addr(&mut self, dev: usize, addr: Ipv4Cidr) -> Result<(), LinuxError> {
        if dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if addr.prefix_len() > 32 || !addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }

        let Some(primary_index) = self.ipv4_addrs.iter().position(|entry| entry.dev == dev) else {
            return self.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr,
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            });
        };
        let old = self.ipv4_addrs[primary_index];
        if old.addr.address() == addr.address() {
            return Ok(());
        }

        if self
            .ipv4_addrs
            .iter()
            .skip(primary_index + 1)
            .any(|entry| entry.dev == dev && entry.addr == addr)
        {
            return Err(LinuxError::EEXIST);
        }
        self.ipv4_addrs[primary_index] = Ipv4AddrEntry {
            dev,
            addr,
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        self.remove_ipv4_source_state_if_unowned(old.addr.address());
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    /// Removes the primary IPv4 address from `dev`.
    ///
    /// Other addresses on the device remain unchanged. An interface without
    /// an IPv4 address is accepted to match `SIOCSIFADDR 0.0.0.0`.
    pub fn remove_primary_ipv4_addr(&mut self, dev: usize) -> Result<(), LinuxError> {
        if dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        let Some(primary_index) = self.ipv4_addrs.iter().position(|entry| entry.dev == dev) else {
            return Ok(());
        };
        let old = self.ipv4_addrs.remove(primary_index);
        self.remove_ipv4_source_state_if_unowned(old.addr.address());
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    pub fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
        let old_len = self.ipv4_addrs.len();
        self.ipv4_addrs
            .retain(|existing| existing.dev != entry.dev || existing.addr != entry.addr);
        if self.ipv4_addrs.len() == old_len {
            return false;
        }

        self.remove_ipv4_source_state_if_unowned(entry.addr.address());
        self.sync_device_ipv4_addrs();
        true
    }

    pub fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == address)
    }

    pub fn has_ipv4_subnet_for_device(&self, dev: usize, address: IpAddress) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.dev == dev && entry.addr.contains_addr(&address))
    }

    pub fn is_ipv4_directed_broadcast_for_device(&self, dev: usize, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev)
            .any(|entry| entry.addr.is_directed_broadcast(address))
    }

    pub fn set_ipv4_broadcast(
        &mut self,
        dev: usize,
        broadcast: Ipv4Address,
    ) -> Result<(), LinuxError> {
        let entry = self
            .ipv4_addrs
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        entry.broadcast = Some(broadcast);
        Ok(())
    }

    /// Updates the prefix length of the primary IPv4 address on `dev`.
    ///
    /// Linux `SIOCSIFNETMASK` keeps `ifa_scope` and only recalculates
    /// `ifa_broadcast` when it still matches the old derived mask.
    pub fn set_ipv4_netmask(&mut self, dev: usize, prefix_len: u8) -> Result<(), LinuxError> {
        if prefix_len > 32 {
            return Err(LinuxError::EINVAL);
        }
        let old = self
            .ipv4_addrs
            .iter()
            .find(|entry| entry.dev == dev)
            .copied()
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        if old.addr.prefix_len() == prefix_len {
            return Ok(());
        }

        let old_derived_broadcast = old.addr.broadcast();
        let has_broadcast_flag = self
            .devices
            .get(dev)
            .is_some_and(|device| device.link_kind() == LinkKind::Ethernet);
        let should_recalculate_broadcast = has_broadcast_flag
            && prefix_len < 31
            && old.effective_broadcast() == old_derived_broadcast;

        let entry = self
            .ipv4_addrs
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        entry.addr = Ipv4Cidr::new(old.addr.address(), prefix_len);
        if should_recalculate_broadcast {
            entry.broadcast = None;
        } else if entry.broadcast.is_none() {
            entry.broadcast = old_derived_broadcast;
        }

        self.sync_device_ipv4_addrs();
        Ok(())
    }

    fn remove_ipv4_source_state_if_unowned(&mut self, address: Ipv4Address) {
        if self.has_ipv4_address(address) {
            return;
        }
        for device in &mut self.devices {
            device.remove_pending_ipv4_source(address);
        }
    }

    pub(crate) fn select_ipv4_source(
        &self,
        dev: usize,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<Ipv4Address> {
        let mut eligible = self
            .ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev && entry.scope <= route_scope);
        let first = eligible.next()?;
        let selected = if first.addr.contains_addr(next_hop) {
            first
        } else {
            eligible
                .find(|entry| entry.addr.contains_addr(next_hop))
                .unwrap_or(first)
        };
        Some(selected.addr.address())
    }

    fn sync_device_ipv4_addrs(&mut self) {
        let local_addrs: Vec<_> = self.ipv4_addrs.iter().map(|entry| entry.addr).collect();
        let assigned_addrs: Vec<Vec<_>> = (0..self.devices.len())
            .map(|dev| {
                self.ipv4_addrs
                    .iter()
                    .filter_map(|entry| (entry.dev == dev).then_some(entry.addr))
                    .collect()
            })
            .collect();
        for (device, assigned_addrs) in self.devices.iter_mut().zip(assigned_addrs) {
            device.set_ipv4_addrs(&assigned_addrs, &local_addrs);
        }
    }
}
