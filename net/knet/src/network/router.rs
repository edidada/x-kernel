// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing rules, route selection, and address-derived routes.

use alloc::vec::Vec;

use kerrno::{KError, KResult, LinuxError};

pub(crate) use crate::netlink::{
    RT_SCOPE_HOST as ROUTE_SCOPE_HOST, RT_SCOPE_LINK as ROUTE_SCOPE_LINK,
    RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE, RT_TABLE_MAIN as ROUTE_TABLE_MAIN,
    RTN_UNICAST as ROUTE_TYPE_UNICAST, RTPROT_BOOT as ROUTE_PROTOCOL_BOOT,
    RTPROT_KERNEL as ROUTE_PROTOCOL_KERNEL,
};
use crate::{
    device::{
        LinkKind,
        interfaces::InterfaceSet,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ipv4AddressRouteKind {
    Local,
    Connected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleSource {
    DevicePrimary,
    Fixed(IpAddress),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleOrigin {
    Configured,
    Ipv4Address {
        owner_dev: usize,
        addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: Retain the route preferred-source attribute \
                  for source-address selection"
    )]
    pub prefsrc: Option<IpAddress>,
    source: RuleSource,
    origin: RuleOrigin,
}

impl Rule {
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: usize, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_BOOT,
            scope: ROUTE_SCOPE_UNIVERSE,
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: Some(src),
            source: RuleSource::Fixed(src),
            origin: RuleOrigin::Configured,
        }
    }

    pub fn with_route_attrs(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: usize,
        src: IpAddress,
        attrs: RouteAttrs,
    ) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            table: attrs.table,
            protocol: attrs.protocol,
            scope: attrs.scope,
            route_type: attrs.route_type,
            prefsrc: attrs.prefsrc,
            source: attrs
                .prefsrc
                .map_or(RuleSource::DevicePrimary, RuleSource::Fixed),
            origin: RuleOrigin::Configured,
        }
    }

    pub(crate) fn for_local_ipv4_address(
        loopback_dev: usize,
        owner_dev: usize,
        owner_addr: Ipv4Cidr,
    ) -> Self {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: 32 is a valid IPv4 prefix length for \
                      the local host route"
        )]
        let filter = Ipv4Cidr::new(owner_addr.address(), 32).unwrap();
        Self::for_ipv4_address(
            filter,
            loopback_dev,
            owner_dev,
            owner_addr,
            Ipv4AddressRouteKind::Local,
            ROUTE_SCOPE_HOST,
        )
    }

    pub(crate) fn for_connected_ipv4_address(dev: usize, owner_addr: Ipv4Cidr, scope: u8) -> Self {
        Self::for_ipv4_address(
            owner_addr.network(),
            dev,
            dev,
            owner_addr,
            Ipv4AddressRouteKind::Connected,
            scope,
        )
    }

    fn for_ipv4_address(
        filter: Ipv4Cidr,
        dev: usize,
        owner_dev: usize,
        owner_addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
        scope: u8,
    ) -> Self {
        let source = IpAddress::Ipv4(owner_addr.address());
        Self {
            filter: filter.into(),
            via: None,
            dev,
            src: source,
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_KERNEL,
            scope,
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: Some(source),
            source: RuleSource::Fixed(source),
            origin: RuleOrigin::Ipv4Address {
                owner_dev,
                addr: owner_addr,
                kind,
            },
        }
    }

    pub(crate) fn preferred_source(self) -> Option<IpAddress> {
        match self.source {
            RuleSource::DevicePrimary => None,
            RuleSource::Fixed(source) => Some(source),
        }
    }

    pub(crate) fn is_configured(self) -> bool {
        matches!(self.origin, RuleOrigin::Configured)
    }

    fn same_route_as(self, other: Self) -> bool {
        self.filter == other.filter
            && self.via == other.via
            && self.dev == other.dev
            && self.table == other.table
            && self.protocol == other.protocol
            && self.scope == other.scope
            && self.route_type == other.route_type
            && self.preferred_source() == other.preferred_source()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteAttrs {
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
}

// TODO(mivik): optimize
struct RouteTable {
    rules: Vec<Rule>,
}
impl RouteTable {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, rule: Rule) {
        let rank = (
            rule.filter.prefix_len(),
            matches!(
                rule.origin,
                RuleOrigin::Ipv4Address {
                    kind: Ipv4AddressRouteKind::Local,
                    ..
                }
            ) as u8,
        );
        let idx = self.rules.partition_point(|existing| {
            let existing_rank = (
                existing.filter.prefix_len(),
                matches!(
                    existing.origin,
                    RuleOrigin::Ipv4Address {
                        kind: Ipv4AddressRouteKind::Local,
                        ..
                    }
                ) as u8,
            );
            existing_rank >= rank
        });
        self.rules.insert(idx, rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn remove_exact_rule(&mut self, route: Rule) {
        if let Some(index) = self.rules.iter().position(|rule| {
            matches!(rule.origin, RuleOrigin::Configured) && rule.same_route_as(route)
        }) {
            self.rules.remove(index);
        }
    }

    fn remove_ipv4_address_routes(&mut self) {
        self.rules
            .retain(|rule| matches!(rule.origin, RuleOrigin::Configured));
    }

    fn remove_configured_routes_with_source(&mut self, source: IpAddress) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured)
                || rule.preferred_source() != Some(source)
        });
    }

    pub fn remove_device(&mut self, dev: usize) {
        self.rules.retain(|rule| {
            rule.dev != dev
                && !matches!(
                    rule.origin,
                    RuleOrigin::Ipv4Address { owner_dev, .. } if owner_dev == dev
                )
        });
        for rule in &mut self.rules {
            if rule.dev > dev {
                rule.dev -= 1;
            }
            if let RuleOrigin::Ipv4Address { owner_dev, .. } = &mut rule.origin
                && *owner_dev > dev
            {
                *owner_dev -= 1;
            }
        }
    }
}

/// Configured and address-derived routes; borrows interfaces for source selection.
pub(crate) struct Router {
    table: RouteTable,
}

impl Router {
    pub(crate) fn new() -> Self {
        Self {
            table: RouteTable::new(),
        }
    }

    /// Returns the most specific route for a destination.
    pub(crate) fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.table.lookup(dst)
    }

    /// Returns the source address for an output route with an active device.
    /// `oif` is a 1-based bound device index; `0` means unbound. A bound
    /// limited broadcast uses that device's primary address without a unicast
    /// route. Missing routes, down devices, and an `oif` mismatch return
    /// `ENETUNREACH`.
    pub fn output_route_source(
        &self,
        interfaces: &InterfaceSet,
        dst: &IpAddress,
        oif: i32,
    ) -> KResult<IpAddress> {
        if dst.is_broadcast() && oif > 0 {
            let device = interfaces
                .devices()
                .get((oif - 1) as usize)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            if !device.is_link_up() {
                return Err(LinuxError::ENETUNREACH.into());
            }
            return Ok(interfaces
                .first_ipv4_addr_for_device(oif as u32)
                .map(|cidr| IpAddress::Ipv4(cidr.address()))
                .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)));
        }
        // Unbound limited broadcast still needs a route so the later
        // dispatch flood has a source address.

        let rule = self
            .table
            .lookup(dst)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if oif > 0 && rule.dev != (oif - 1) as usize {
            return Err(KError::from(LinuxError::ENETUNREACH));
        }
        let device = interfaces
            .devices()
            .get(rule.dev)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        match rule.source {
            RuleSource::Fixed(source) => Ok(source),
            RuleSource::DevicePrimary => interfaces
                .select_ipv4_source(rule.dev, &rule.via.unwrap_or(*dst), rule.scope)
                .map(IpAddress::Ipv4)
                .ok_or(KError::from(LinuxError::ENETUNREACH)),
        }
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    /// Returns the first matching rule without copying the complete routing table.
    pub(crate) fn find_rule(&self, mut matches_fn: impl FnMut(&Rule) -> bool) -> Option<Rule> {
        self.table
            .rules()
            .iter()
            .find(|rule| matches_fn(rule))
            .copied()
    }

    pub fn route_snapshot(&self) -> Vec<Rule> {
        self.table.rules().to_vec()
    }

    pub fn add_route_rule(
        &mut self,
        interfaces: &InterfaceSet,
        rule: Rule,
    ) -> Result<(), LinuxError> {
        self.validate_configured_route(interfaces, rule)?;
        self.table.add_rule(rule);
        Ok(())
    }

    pub fn replace_route_rule(
        &mut self,
        interfaces: &InterfaceSet,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.validate_configured_route(interfaces, replacement)?;
        self.table.remove_exact_rule(existing);
        self.table.add_rule(replacement);
        Ok(())
    }

    pub fn remove_exact_route_rule(&mut self, route: Rule) {
        self.table.remove_exact_rule(route);
    }

    fn validate_configured_route(
        &self,
        interfaces: &InterfaceSet,
        rule: Rule,
    ) -> Result<(), LinuxError> {
        if !rule.is_configured() {
            return Err(LinuxError::EINVAL);
        }
        if rule.table != ROUTE_TABLE_MAIN || rule.route_type != ROUTE_TYPE_UNICAST {
            return Err(LinuxError::EOPNOTSUPP);
        }
        if !matches!(rule.filter, IpCidr::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        if rule.dev >= interfaces.devices().len() {
            return Err(LinuxError::ENODEV);
        }
        if let Some(prefsrc) = rule.preferred_source()
            && !matches!(prefsrc, IpAddress::Ipv4(address) if interfaces.has_ipv4_address(address))
        {
            return Err(LinuxError::EINVAL);
        }
        if let Some(gateway) = rule.via {
            let IpAddress::Ipv4(gateway) = gateway else {
                return Err(LinuxError::EAFNOSUPPORT);
            };
            if !gateway.is_unicast()
                || interfaces.is_ipv4_directed_broadcast_for_device(rule.dev, gateway)
            {
                return Err(LinuxError::EINVAL);
            }
            if !interfaces.has_ipv4_subnet_for_device(rule.dev, gateway.into()) {
                return Err(LinuxError::ENETUNREACH);
            }
        }
        Ok(())
    }

    pub(crate) fn rebuild_ipv4_address_routes(&mut self, interfaces: &InterfaceSet) {
        self.table.remove_ipv4_address_routes();
        let loopback_dev = interfaces
            .devices()
            .iter()
            .position(|device| device.link_kind() == crate::device::LinkKind::Loopback);
        let entries = interfaces.ipv4_addr_entries().iter().copied();
        let mut connected = Vec::new();
        for entry in entries {
            if let Some(loopback_dev) = loopback_dev {
                self.table.add_rule(Rule::for_local_ipv4_address(
                    loopback_dev,
                    entry.dev,
                    entry.addr,
                ));
            }

            let network = entry.addr.network();
            if entry.addr.prefix_len() == 32
                || network.address().is_unspecified()
                || connected.contains(&(entry.dev, network))
            {
                continue;
            }
            connected.push((entry.dev, network));
            let scope = if interfaces.devices()[entry.dev].link_kind()
                == crate::device::LinkKind::Loopback
            {
                ROUTE_SCOPE_HOST
            } else {
                ROUTE_SCOPE_LINK
            };
            self.table.add_rule(Rule::for_connected_ipv4_address(
                entry.dev,
                entry.addr,
                scope,
            ));
        }
    }

    /// Removes routes referencing a removed interface and renumbers later slots.
    pub(crate) fn remove_device_routes(&mut self, index: usize) {
        self.table.remove_device(index);
    }

    /// Removes configured routes that require a source address whose last owner left.
    pub(crate) fn remove_routes_with_source(&mut self, source: Ipv4Address) {
        self.table
            .remove_configured_routes_with_source(source.into());
    }
}
