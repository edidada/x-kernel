// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 address assignments to network interfaces.
//!
//! Each record combines address values from `crate::ip` with an owning
//! interface, scope, and optional broadcast override. `Ipv4AddressSet` owns
//! their ordering, insertion checks, capacity, and primary-address update rules.

use alloc::{string::String, vec::Vec};

use kerrno::LinuxError;

use crate::{
    device::NetDeviceId,
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
};

/// An IPv4 address configured on a network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ipv4AddrEntry {
    pub dev: NetDeviceId,
    pub addr: Ipv4Cidr,
    pub scope: u8,
    /// Linux `ifa_broadcast` override. `None` means derive from the CIDR.
    pub broadcast: Option<Ipv4Address>,
}

impl Ipv4AddrEntry {
    pub(crate) fn effective_broadcast(self) -> Option<Ipv4Address> {
        self.broadcast.or_else(|| self.addr.broadcast())
    }
}

/// An interface address and its interface label for configuration queries.
#[derive(Clone, Debug)]
pub(crate) struct Ipv4AddrSnapshot {
    pub entry: Ipv4AddrEntry,
    pub label: String,
}

/// The result of updating an interface's primary IPv4 address.
pub(crate) enum PrimaryAddrChange {
    Unchanged,
    Added,
    /// The old local address whose dependent state may need cleanup.
    Replaced(Ipv4Address),
}

/// Ordered interface addresses with a caller-supplied capacity limit.
///
/// The first entry for each device is its primary address. Mutations preserve
/// other entries' order, so removing the primary promotes the next entry.
/// Callers validate device existence and synchronize derived network state.
pub(crate) struct Ipv4AddressSet {
    entries: Vec<Ipv4AddrEntry>,
    max_entries: usize,
}

impl Ipv4AddressSet {
    pub(crate) fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries,
        }
    }

    pub(crate) fn as_slice(&self) -> &[Ipv4AddrEntry] {
        &self.entries
    }

    pub(crate) fn iter(&self) -> core::slice::Iter<'_, Ipv4AddrEntry> {
        self.entries.iter()
    }

    pub(crate) fn contains(&self, address: Ipv4Address) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.addr.address() == address)
    }

    /// Tests address identity by device and CIDR, excluding mutable attributes.
    pub(crate) fn contains_entry(&self, entry: Ipv4AddrEntry) -> bool {
        self.entries
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
    }

    pub(crate) fn primary(&self, dev: NetDeviceId) -> Option<Ipv4Cidr> {
        self.entries
            .iter()
            .find(|entry| entry.dev == dev)
            .map(|entry| entry.addr)
    }

    /// Adds an address, returning whether a new device/CIDR pair was inserted.
    pub(crate) fn insert(&mut self, entry: Ipv4AddrEntry) -> Result<bool, LinuxError> {
        if !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        if self.contains_entry(entry) {
            return Ok(false);
        }
        if self.entries.len() >= self.max_entries {
            return Err(LinuxError::ENOBUFS);
        }
        self.entries.push(entry);
        Ok(true)
    }

    /// Replaces the primary address with the caller-supplied entry.
    /// Setting the same local address preserves all existing attributes.
    pub(crate) fn set_primary(
        &mut self,
        entry: Ipv4AddrEntry,
    ) -> Result<PrimaryAddrChange, LinuxError> {
        if !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        let Some(index) = self
            .entries
            .iter()
            .position(|existing| existing.dev == entry.dev)
        else {
            self.insert(entry)?;
            return Ok(PrimaryAddrChange::Added);
        };
        let old = self.entries[index];
        if old.addr.address() == entry.addr.address() {
            return Ok(PrimaryAddrChange::Unchanged);
        }
        if self
            .entries
            .iter()
            .skip(index + 1)
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            return Err(LinuxError::EEXIST);
        }
        self.entries[index] = entry;
        Ok(PrimaryAddrChange::Replaced(old.addr.address()))
    }

    pub(crate) fn remove_primary(&mut self, dev: NetDeviceId) -> Option<Ipv4AddrEntry> {
        let index = self.entries.iter().position(|entry| entry.dev == dev)?;
        Some(self.entries.remove(index))
    }

    pub(crate) fn remove(&mut self, entry: Ipv4AddrEntry) -> bool {
        let old_len = self.entries.len();
        self.entries
            .retain(|existing| existing.dev != entry.dev || existing.addr != entry.addr);
        self.entries.len() != old_len
    }

    /// Removes a device's addresses and returns the source values to reconcile.
    pub(crate) fn remove_device(&mut self, dev: NetDeviceId) -> Vec<Ipv4Address> {
        let removed = self
            .entries
            .iter()
            .filter_map(|entry| (entry.dev == dev).then_some(entry.addr.address()))
            .collect();
        self.entries.retain(|entry| entry.dev != dev);
        removed
    }

    pub(crate) fn set_broadcast(
        &mut self,
        dev: NetDeviceId,
        broadcast: Ipv4Address,
    ) -> Result<(), LinuxError> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        entry.broadcast = Some(broadcast);
        Ok(())
    }

    /// Preserves scope and custom broadcasts when changing the primary prefix.
    /// Returns whether the prefix changed.
    pub(crate) fn set_netmask(
        &mut self,
        dev: NetDeviceId,
        prefix_len: u8,
        has_broadcast_flag: bool,
    ) -> Result<bool, LinuxError> {
        if prefix_len > 32 {
            return Err(LinuxError::EINVAL);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.dev == dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        if entry.addr.prefix_len() == prefix_len {
            return Ok(false);
        }
        let old_derived_broadcast = entry.addr.broadcast();
        let should_recalculate_broadcast = has_broadcast_flag
            && prefix_len < 31
            && entry.effective_broadcast() == old_derived_broadcast;
        entry.addr =
            Ipv4Cidr::new(entry.addr.address(), prefix_len).expect("prefix length was validated");
        if should_recalculate_broadcast {
            entry.broadcast = None;
        } else if entry.broadcast.is_none() {
            entry.broadcast = old_derived_broadcast;
        }
        Ok(true)
    }

    /// Prefers an address covering the next hop among entries eligible by scope,
    /// falling back to the first eligible address on the selected device.
    pub(crate) fn select_source(
        &self,
        dev: NetDeviceId,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<Ipv4Address> {
        let mut eligible = self
            .entries
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
}
