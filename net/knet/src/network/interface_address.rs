// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 address assignments to network interfaces.
//!
//! Each record combines address values from `crate::ip` with an owning
//! interface, scope, and optional broadcast override. `Ipv4AddressSet` owns
//! their ordering, identity checks, capacity, and source-address selection.

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

/// Ordered interface addresses with a caller-supplied capacity limit.
///
/// The first entry for each device is its default address, called `primary`
/// by this API. This ordering does not model Linux `IFA_F_SECONDARY` semantics.
/// Mutations preserve other entries' order, so removing the default address
/// makes the next entry the default.
/// Device/CIDR pairs are unique, and rejected mutations leave all entries intact.
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
        self.entry_index(entry.dev, entry.addr).is_some()
    }

    fn entry_index(&self, dev: NetDeviceId, addr: Ipv4Cidr) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.dev == dev && entry.addr == addr)
    }

    pub(crate) fn primary(&self, dev: NetDeviceId) -> Option<Ipv4Cidr> {
        self.primary_entry(dev).map(|entry| entry.addr)
    }

    /// Returns a copy of the first address entry for the device.
    pub(crate) fn primary_entry(&self, dev: NetDeviceId) -> Option<Ipv4AddrEntry> {
        self.entries.iter().find(|entry| entry.dev == dev).copied()
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

    /// Replaces all attributes of the device's primary entry and returns the old entry.
    ///
    /// Preserves the device and entry order. Returns `EADDRNOTAVAIL` when the
    /// device has no address, `EINVAL` for a non-unicast local address, or
    /// `EEXIST` for a conflicting device/CIDR pair. Errors leave entries intact.
    pub(crate) fn replace_primary(
        &mut self,
        entry: Ipv4AddrEntry,
    ) -> Result<Ipv4AddrEntry, LinuxError> {
        let index = self
            .entries
            .iter()
            .position(|existing| existing.dev == entry.dev)
            .ok_or(LinuxError::EADDRNOTAVAIL)?;
        self.replace_at(index, entry.addr, entry.scope, entry.broadcast)
    }

    /// Validates a replacement before committing it without changing its device.
    fn replace_at(
        &mut self,
        index: usize,
        addr: Ipv4Cidr,
        scope: u8,
        broadcast: Option<Ipv4Address>,
    ) -> Result<Ipv4AddrEntry, LinuxError> {
        if !addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        let old = self.entries[index];
        if self
            .entry_index(old.dev, addr)
            .is_some_and(|existing_index| existing_index != index)
        {
            return Err(LinuxError::EEXIST);
        }
        self.entries[index] = Ipv4AddrEntry {
            dev: old.dev,
            addr,
            scope,
            broadcast,
        };
        Ok(old)
    }

    pub(crate) fn remove_primary(&mut self, dev: NetDeviceId) -> Option<Ipv4AddrEntry> {
        let index = self.entries.iter().position(|entry| entry.dev == dev)?;
        Some(self.entries.remove(index))
    }

    pub(crate) fn remove(&mut self, entry: Ipv4AddrEntry) -> bool {
        let Some(index) = self.entry_index(entry.dev, entry.addr) else {
            return false;
        };
        self.entries.remove(index);
        true
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

#[unittest::mod_test]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn replacing_primary_entry_updates_all_attributes_and_preserves_other_entries() {
        let mut addresses = Ipv4AddressSet::new(3);
        let primary = Ipv4AddrEntry {
            dev: NetDeviceId::new(2).unwrap(),
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24).unwrap(),
            scope: 254,
            broadcast: None,
        };
        let other_device = Ipv4AddrEntry {
            dev: NetDeviceId::new(3).unwrap(),
            addr: Ipv4Cidr::new(primary.addr.address(), 16).unwrap(),
            ..primary
        };
        let secondary = Ipv4AddrEntry {
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24).unwrap(),
            ..primary
        };
        for entry in [primary, other_device, secondary] {
            addresses.insert(entry).unwrap();
        }

        let replacement = Ipv4AddrEntry {
            scope: 0,
            broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
            ..primary
        };
        assert_eq!(addresses.replace_primary(replacement), Ok(primary));
        let updated_prefix = Ipv4AddrEntry {
            addr: other_device.addr,
            ..replacement
        };
        assert_eq!(addresses.replace_primary(updated_prefix), Ok(replacement));
        assert_eq!(addresses.primary_entry(primary.dev), Some(updated_prefix));
        assert_eq!(
            addresses.as_slice(),
            &[updated_prefix, other_device, secondary]
        );

        for (entry, error) in [
            (secondary, LinuxError::EEXIST),
            (
                Ipv4AddrEntry {
                    addr: Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 32).unwrap(),
                    ..updated_prefix
                },
                LinuxError::EINVAL,
            ),
            (
                Ipv4AddrEntry {
                    dev: NetDeviceId::new(4).unwrap(),
                    ..updated_prefix
                },
                LinuxError::EADDRNOTAVAIL,
            ),
        ] {
            assert_eq!(addresses.replace_primary(entry), Err(error));
            assert_eq!(
                addresses.as_slice(),
                &[updated_prefix, other_device, secondary]
            );
        }
    }
}
