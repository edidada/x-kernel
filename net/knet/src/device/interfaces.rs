// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interface inventory, link configuration, and neighbor updates.

use alloc::{boxed::Box, string::String, vec::Vec};

use kerrno::LinuxError;

use super::{
    DeviceHandle, LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate,
    NeighborUpdatePolicy, NetDevice,
};
use crate::{
    consts::STANDARD_MTU,
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    netlink::RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE,
};

/// Device and IPv4 address inventory protected by [`crate::network::state::NetworkState`].
pub(crate) struct InterfaceSet {
    devices: Vec<DeviceHandle>,
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
    /// Borrows the live device handles in their current slot order.
    pub(crate) fn devices(&self) -> &[DeviceHandle] {
        &self.devices
    }

    pub(crate) fn has_rx_work(&self) -> bool {
        self.devices.iter().any(|device| device.has_rx_work())
    }

    pub(crate) fn new() -> Self {
        Self {
            devices: Vec::new(),
            ipv4_addrs: Vec::new(),
            effective_mtu: STANDARD_MTU,
        }
    }

    /// Returns the MTU advertised through the smoltcp device capabilities.
    pub(crate) fn effective_mtu(&self) -> usize {
        self.effective_mtu
    }

    pub(crate) fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.devices
            .iter()
            .enumerate()
            .map(|(dev, device)| device.link_snapshot(dev as i32 + 1))
            .collect()
    }

    pub(crate) fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub(crate) fn has_device(&self, ifindex: i32) -> bool {
        Self::device_index(ifindex).is_some_and(|dev_index| self.devices.get(dev_index).is_some())
    }

    pub(crate) fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.devices.iter().position(|device| device.name() == name)
    }

    pub(crate) fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_send_snapshot())
    }

    /// Applies a validated link update.
    ///
    /// Returns `Ok(true)` when the effective device MTU changed and the
    /// smoltcp interface must be rebuilt to refresh its cached capabilities.
    pub(crate) fn update_device_link(
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
        self.devices[dev].apply_link_config(name, mtu, is_up)?;
        if is_mtu_changed {
            self.recompute_effective_mtu();
        }
        Ok(self.effective_mtu != effective_mtu)
    }

    fn device_index(ifindex: i32) -> Option<usize> {
        (ifindex > 0).then_some((ifindex - 1) as usize)
    }

    pub(crate) fn apply_neighbor_update(
        &mut self,
        update: NeighborUpdate,
        policy: NeighborUpdatePolicy,
    ) -> Result<(), LinuxError> {
        let device = self.devices.get(update.dev).ok_or(LinuxError::ENODEV)?;
        device.apply_neighbor_update(update, policy)
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
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

    pub(crate) fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(DeviceHandle::new(device));
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
        self.devices[pos].with_mut(|device| device.set_link_up(false));
        self.devices.remove(pos);
        self.ipv4_addrs.retain(|entry| entry.dev != pos);
        for entry in &mut self.ipv4_addrs {
            if entry.dev > pos {
                entry.dev -= 1;
            }
        }
        for source in removed_sources {
            if !self.has_ipv4_address(source) {
                for device in &self.devices {
                    device.remove_pending_ipv4_source(source);
                }
            }
        }
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        Some(pos)
    }

    pub(crate) fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
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

    /// Returns the primary IPv4 address of `name`, with device resolution and
    /// lookup under one borrow so removal cannot renumber slots between them.
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

    pub(crate) fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        &self.ipv4_addrs
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.ipv4_addrs
            .iter()
            .filter_map(|entry| {
                self.devices.get(entry.dev).map(|device| Ipv4AddrSnapshot {
                    entry: *entry,
                    label: device.name(),
                })
            })
            .collect()
    }

    pub(crate) fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
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

    /// Updates the primary IPv4 address on `dev`; other addresses remain unchanged.
    /// Updating an existing address resets its scope and broadcast fields.
    pub(crate) fn set_primary_ipv4_addr(
        &mut self,
        dev: usize,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
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

    /// Removes the primary IPv4 address from `dev`; other addresses remain
    /// unchanged and a device without one is accepted.
    pub(crate) fn remove_primary_ipv4_addr(&mut self, dev: usize) -> Result<(), LinuxError> {
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

    pub(crate) fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
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

    pub(crate) fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == address)
    }

    pub(crate) fn has_ipv4_subnet_for_device(&self, dev: usize, address: IpAddress) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.dev == dev && entry.addr.contains_addr(&address))
    }

    pub(crate) fn is_ipv4_directed_broadcast_for_device(
        &self,
        dev: usize,
        address: Ipv4Address,
    ) -> bool {
        self.ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev)
            .any(|entry| entry.addr.is_directed_broadcast(address))
    }

    pub(crate) fn set_ipv4_broadcast(
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

    /// Updates the prefix length of the primary IPv4 address on `dev`, keeping
    /// its scope and recomputing a mask-derived broadcast only.
    pub(crate) fn set_ipv4_netmask(
        &mut self,
        dev: usize,
        prefix_len: u8,
    ) -> Result<(), LinuxError> {
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
        entry.addr = Ipv4Cidr::new(old.addr.address(), prefix_len).ok_or(LinuxError::EINVAL)?;
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
        for device in &self.devices {
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
        for (device, assigned_addrs) in self.devices.iter().zip(assigned_addrs) {
            device.set_ipv4_addrs(&assigned_addrs, &local_addrs);
        }
    }
}
