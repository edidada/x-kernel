// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 output route resolution.
//!
//! Owns the read-only routing queries over [`RouteTable`] and
//! [`DeviceRegistry`]:
//!
//! - [`resolve_output_route`] and [`output_source_address`] resolve the active
//!   output device, next hop, source address, and MTU for data-plane packet
//!   preparation, queued packet dispatch, and socket source-address queries.
//! - [`Ipv4OutputLookup`] supplies the shared lookup for these paths and
//!   control-plane `RTM_GETROUTE` queries,
//!   including Linux's on-link synthesis, `0.0.0.0` loopback rewrite, and
//!   `RTM_F_FIB_MATCH` metadata.
//!
//! Keeping both in the routing layer lets [`crate::stack::router::Router`] and
//! [`super::output`] depend on one routing boundary without [`Router`]
//! depending back on packet-output state.

use kerrno::{KResult, LinuxError};

use crate::{
    device::{DeviceRegistry, LinkKind, NetDeviceId},
    ip::{IpAddress, Ipv4Address},
    network::router::{
        ROUTE_SCOPE_HOST, ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE, ROUTE_TYPE_BROADCAST,
        ROUTE_TYPE_LOCAL, ROUTE_TYPE_MULTICAST, ROUTE_TYPE_UNICAST, RouteTable, Rule,
    },
};

/// An active IPv4 output route decision for packet transmission, consumed under
/// one configuration guard.
pub(crate) struct OutputRoute {
    pub(crate) destination: Ipv4Address,
    pub(crate) source: Ipv4Address,
    pub(crate) dev: NetDeviceId,
    pub(crate) next_hop: Ipv4Address,
    pub(crate) mtu: usize,
    pub(crate) link_kind: LinkKind,
}

/// Resolves the route and its active output device without touching packet queues.
///
/// `oif` constrains the lookup to a bound interface when present. Returns
/// `ENETUNREACH` for a missing route, absent or down device, mismatched
/// binding, or unavailable source address. The caller must serialize
/// configuration access and use of the result in the same critical section.
/// An explicit `bound_src` must be local, may belong to another device, and
/// takes precedence over preferred or automatically selected source addresses.
/// Destination rewriting, source validation, and interface fallback use
/// [`Ipv4OutputLookup::lookup`], including its `EINVAL` and `ENODEV` errors.
pub(crate) fn resolve_output_route(
    routes: &RouteTable,
    devices: &DeviceRegistry,
    dst: Ipv4Address,
    oif: Option<NetDeviceId>,
    bound_src: Option<Ipv4Address>,
) -> KResult<OutputRoute> {
    let route = Ipv4OutputLookup::new(routes, devices).lookup(dst, bound_src, oif)?;
    let device = devices
        .get_by_id(route.oif)
        .ok_or(LinuxError::ENETUNREACH)?;
    if !device.is_link_up() {
        return Err(LinuxError::ENETUNREACH.into());
    }
    Ok(OutputRoute {
        destination: route.destination,
        source: route.source.ok_or(LinuxError::ENETUNREACH)?,
        dev: route.oif,
        next_hop: route.gateway.unwrap_or(route.destination),
        mtu: device.mtu().min(usize::from(u16::MAX)),
        link_kind: device.link_kind(),
    })
}

/// Selects a local output source, including bound route-less broadcasts.
///
/// An explicit source is validated without requiring a default source. With no
/// explicit source, bound limited broadcasts may use `0.0.0.0` on an
/// unaddressed device.
pub(crate) fn output_source_address(
    routes: &RouteTable,
    devices: &DeviceRegistry,
    dst: Ipv4Address,
    oif: Option<NetDeviceId>,
    bound_src: Option<Ipv4Address>,
) -> KResult<Ipv4Address> {
    Ok(resolve_output_route(routes, devices, dst, oif, bound_src)?.source)
}

/// Returns whether `address` is configured on any local interface.
pub(crate) fn has_local_source(devices: &DeviceRegistry, address: Ipv4Address) -> bool {
    address_owner(devices, address).is_some()
}

fn address_owner(devices: &DeviceRegistry, addr: Ipv4Address) -> Option<NetDeviceId> {
    devices
        .iter_ipv4_addrs()
        .find(|(_, entry)| entry.addr.address() == addr)
        .map(|(dev, _)| dev)
}

/// Result of the output-route lookup performed for `RTM_GETROUTE`.
///
/// `fib_entry` is absent only for Linux's source-specified multicast
/// shortcut, zero-destination loopback rewrite, and on-link synthesis.  The
/// source shortcut bypasses the FIB; the other two paths start with an empty
/// `fib_result`.  FIB hits keep their entry even when final output turns the
/// route into a broadcast or multicast route, because `RTM_F_FIB_MATCH`
/// reports the matched FIB entry rather than the constructed output type.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ipv4OutputRoute {
    /// Effective destination after resolving an unspecified target.
    pub(crate) destination: Ipv4Address,
    pub(crate) fib_entry: Option<Rule>,
    pub(crate) table: u8,
    pub(crate) route_type: u8,
    pub(crate) oif: NetDeviceId,
    pub(crate) source: Option<Ipv4Address>,
    pub(crate) gateway: Option<Ipv4Address>,
}

fn ipv4_address(addr: IpAddress) -> Option<Ipv4Address> {
    match addr {
        IpAddress::Ipv4(addr) => Some(addr),
        IpAddress::Ipv6(_) => None,
    }
}

/// Read-only routing view over one consistent configuration snapshot.
///
/// The caller holds the Router lock that owns both references for as long as
/// it uses the returned route.
pub(crate) struct Ipv4OutputLookup<'a> {
    table: &'a RouteTable,
    devices: &'a DeviceRegistry,
}

impl<'a> Ipv4OutputLookup<'a> {
    pub(crate) fn new(table: &'a RouteTable, devices: &'a DeviceRegistry) -> Self {
        Self { table, devices }
    }

    /// Resolves the effective IPv4 destination, source, and output route.
    ///
    /// This follows Linux's output lookup order: validate an explicit source,
    /// apply the output-interface shortcuts, handle the zero-destination
    /// loopback rewrite, and only then consult the routing table.  The lookup
    /// itself has no `FLOWI_FLAG_ANYSRC`; an explicit source must be local.
    ///
    /// An explicit `oif` constrains the FIB lookup and, when no route matches,
    /// synthesizes an on-link route exactly as `ip_route_output_key_hash_rcu`
    /// does.  The project does not model policy rules, marks, UIDs, protocol
    /// or port based routing, input routes, multipath, or metrics; those
    /// selectors are accepted but do not change the matched route.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for a multicast, limited-broadcast, or zero-network
    /// source, `ENETUNREACH` for a non-local source or an unreachable route,
    /// and `ENODEV` when an explicit output interface does not exist.
    pub(crate) fn lookup(
        &self,
        dst: Ipv4Address,
        src: Option<Ipv4Address>,
        oif: Option<NetDeviceId>,
    ) -> KResult<Ipv4OutputRoute> {
        let requested = src.filter(|src| !src.is_unspecified());
        if let Some(src) = requested {
            if src.is_multicast() || src.is_broadcast() || src.is_zeronet() {
                return Err(LinuxError::EINVAL.into());
            }
            if !has_local_source(self.devices, src) {
                return Err(LinuxError::ENETUNREACH.into());
            }
        }

        let output_oif = oif;
        if let Some(id) = output_oif {
            let device = self.devices.get_by_id(id).ok_or(LinuxError::ENODEV)?;
            if !device.is_link_up() || self.devices.ipv4(id).is_none() {
                return Err(LinuxError::ENETUNREACH.into());
            }
        }

        if let Some(route) = self.output_shortcut(dst, requested, output_oif)? {
            return self.finish_output_route(route, dst);
        }

        // Linux preselects an output-interface source only for the zero
        // destination (host scope) and multicast (universe scope).  Ordinary
        // output routes leave `fl4.saddr` unset until `fib_select_path`, so a
        // route's preferred source is not shadowed by a device-primary source.
        let mut flow_source = requested;
        if flow_source.is_none()
            && let Some(output) = output_oif
        {
            if dst.is_unspecified() {
                flow_source =
                    self.select_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_HOST);
            } else if dst.is_multicast() {
                flow_source =
                    self.select_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_UNIVERSE);
            }
        }

        let zero_destination = dst.is_unspecified();
        let lookup_dst = if zero_destination {
            flow_source.unwrap_or(Ipv4Address::LOOPBACK)
        } else {
            dst
        };

        let rule = self
            .table
            .lookup_for_output(&IpAddress::Ipv4(lookup_dst), output_oif);

        let route = if zero_destination {
            Ipv4OutputRoute {
                destination: lookup_dst,
                fib_entry: None,
                table: 0,
                route_type: ROUTE_TYPE_LOCAL,
                oif: NetDeviceId::LOOPBACK,
                source: flow_source.or(Some(lookup_dst)),
                gateway: None,
            }
        } else if let Some(rule) = rule {
            self.output_route_from_fib(*rule, dst, flow_source)
        } else if let Some(output) = output_oif {
            Ipv4OutputRoute {
                destination: dst,
                fib_entry: None,
                table: 0,
                route_type: ROUTE_TYPE_UNICAST,
                oif: output,
                source: flow_source.or_else(|| {
                    self.select_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_LINK)
                }),
                gateway: None,
            }
        } else {
            return Err(LinuxError::ENETUNREACH.into());
        };

        self.finish_output_route(route, dst)
    }

    /// Applies the two output lookups that Linux resolves without consulting
    /// the FIB: an explicit multicast/broadcast source selects the owning
    /// device, and an explicit output interface selects a local multicast or
    /// limited-broadcast route.  IGMP is unreachable because this lookup has
    /// no protocol selector.
    fn output_shortcut(
        &self,
        dst: Ipv4Address,
        requested: Option<Ipv4Address>,
        output_oif: Option<NetDeviceId>,
    ) -> KResult<Option<Ipv4OutputRoute>> {
        if output_oif.is_none()
            && (dst.is_multicast() || dst.is_broadcast())
            && let Some(source) = requested
        {
            let output = address_owner(self.devices, source).ok_or(LinuxError::ENETUNREACH)?;
            return Ok(Some(Ipv4OutputRoute {
                destination: dst,
                fib_entry: None,
                table: 0,
                route_type: 0,
                oif: output,
                source: Some(source),
                gateway: None,
            }));
        }

        if let Some(output) = output_oif
            && (dst.is_local_multicast() || dst.is_broadcast())
        {
            let source = requested
                .or_else(|| self.select_source(output, Ipv4Address::UNSPECIFIED, ROUTE_SCOPE_LINK))
                .unwrap_or(Ipv4Address::UNSPECIFIED);
            return Ok(Some(Ipv4OutputRoute {
                destination: dst,
                fib_entry: None,
                table: 0,
                route_type: 0,
                oif: output,
                source: Some(source),
                gateway: None,
            }));
        }

        Ok(None)
    }

    /// Builds the output route for a matched FIB rule.  A local rule is served
    /// by the loopback device; other rules select their configured preferred
    /// source before falling back to address selection.
    fn output_route_from_fib(
        &self,
        rule: Rule,
        dst: Ipv4Address,
        flow_source: Option<Ipv4Address>,
    ) -> Ipv4OutputRoute {
        let (table, route_type, _) = rule.notification_route();
        let mut output_oif = rule.lookup_device();
        let mut selected_source = flow_source;
        if route_type == ROUTE_TYPE_LOCAL {
            if selected_source.is_none() {
                selected_source = rule.preferred_source().and_then(ipv4_address).or(Some(dst));
            }
            output_oif = NetDeviceId::LOOPBACK;
        } else if selected_source.is_none() {
            selected_source = rule.preferred_source().and_then(ipv4_address).or_else(|| {
                self.select_source(
                    output_oif,
                    rule.via
                        .and_then(ipv4_address)
                        .unwrap_or(Ipv4Address::UNSPECIFIED),
                    rule.scope,
                )
            });
        }
        Ipv4OutputRoute {
            destination: dst,
            fib_entry: Some(rule),
            table,
            route_type,
            oif: output_oif,
            source: selected_source,
            gateway: if route_type == ROUTE_TYPE_LOCAL {
                None
            } else {
                rule.via.and_then(ipv4_address)
            },
        }
    }

    fn finish_output_route(
        &self,
        mut route: Ipv4OutputRoute,
        requested_dst: Ipv4Address,
    ) -> KResult<Ipv4OutputRoute> {
        if let Some(source) = route.source
            && source.is_loopback()
            && route.oif != NetDeviceId::LOOPBACK
        {
            return Err(LinuxError::EINVAL.into());
        }
        if requested_dst.is_multicast() {
            route.route_type = ROUTE_TYPE_MULTICAST;
            // `__mkroute_output` drops a multicast FIB result shorter than
            // the IPv4 multicast prefix, so the packet goes out on the link
            // without a gateway.  The FIB entry itself survives for
            // `RTM_F_FIB_MATCH` replies.
            if route
                .fib_entry
                .is_some_and(|fib| fib.filter.prefix_len() < 4)
            {
                route.gateway = None;
            }
        } else if requested_dst.is_broadcast() {
            route.route_type = ROUTE_TYPE_BROADCAST;
            // Linux unconditionally clears `fi` for limited broadcast.
            route.gateway = None;
        }
        Ok(route)
    }

    fn select_source(
        &self,
        dev: NetDeviceId,
        next_hop: Ipv4Address,
        scope: u8,
    ) -> Option<Ipv4Address> {
        self.devices
            .ipv4(dev)
            .and_then(|addrs| addrs.select_source(&IpAddress::Ipv4(next_hop), scope))
            .or_else(|| {
                // Linux continues through every device in `dev_base` order
                // when the output device has no eligible primary address.
                self.devices
                    .iter_ipv4_addrs()
                    .find_map(|(dev, _)| self.devices.ipv4(dev)?.select_fallback_source(scope))
            })
    }
}
