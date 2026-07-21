// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! It provides unified networking primitives for TCP/UDP communication.
//!
//! # Organization
//!
//! - [`TcpSocket`]: A TCP socket that provides POSIX-like APIs.
//! - [`UdpSocket`]: A UDP socket that provides POSIX-like APIs.
//! - [`dns_query`]: Function for DNS query.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

#[macro_use]
extern crate log;
extern crate alloc;

mod consts;
mod device;
mod ip;
mod link;
pub mod netlink;
mod socket;
mod stack;
mod transport;
pub mod unix;
#[cfg(feature = "vsock")]
pub mod vsock;

use alloc::{borrow::ToOwned, boxed::Box, sync::Arc};

#[cfg(feature = "vsock")]
use kclass::{ClassDevice, VsockDeviceImpl, subscribe_vsock_available, vsock_devices};
use kclass::{NetDevice, net_devices};
use kdevice::subscribe_device_removed;
use ksync::Mutex;
use lazyinit::LazyInit;
pub use link::packet;
pub(crate) use link::{buf, wire};
pub use socket::{
    file::{sock_alloc_file, sock_from_file},
    options, *,
};
pub(crate) use socket::{general, state};
pub(crate) use stack::{ipv4, listen_table, router, service, wrapper};
pub(crate) use transport::udp_err;
pub use transport::{raw, tcp, udp};

use crate::{
    consts::{GATEWAY, IP, IP_PREFIX, STANDARD_MTU},
    device::{EthernetDevice, LoopbackDevice},
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    listen_table::ListenTable,
    router::{
        Ipv4AddrEntry, ROUTE_PROTOCOL_BOOT, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE,
        ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs, Router, Rule,
    },
    service::Service,
    wrapper::SocketSetWrapper,
};

static LISTEN_TABLE: LazyInit<ListenTable> = LazyInit::new();
static SOCKET_SET: LazyInit<SocketSetWrapper> = LazyInit::new();

static SERVICE: LazyInit<Mutex<Service>> = LazyInit::new();

/// Initializes the network subsystem by NIC devices.
pub fn init_network() {
    info!("Initialize network subsystem...");

    let mut net_devs = net_devices();

    let mut router = Router::new();
    let lo_dev = router.add_device(Box::new(LoopbackDevice::new()));

    let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    router.add_ipv4_addr(Ipv4AddrEntry {
        dev: lo_dev,
        addr: lo_ip,
        scope: ROUTE_SCOPE_HOST,
    });
    router.add_route_rule(Rule::with_route_attrs(
        lo_ip.into(),
        None,
        lo_dev,
        lo_ip.address().into(),
        65_536,
        RouteAttrs {
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_BOOT,
            scope: ROUTE_SCOPE_HOST,
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: Some(lo_ip.address().into()),
        },
    ));

    let mut eth0_irq = None;
    if let Some(handle) = net_devs.pop() {
        let device_id = handle.id();
        info!("  use NIC 0: {:?}", handle.name());

        let eth0_address = wire::MacAddress(handle.mac().0);
        eth0_irq = handle.irq();
        let eth0_ip = Ipv4Cidr::new(IP.parse().expect("Invalid IPv4 address"), IP_PREFIX);

        let eth0_dev = router.add_device(Box::new(EthernetDevice::new(
            "eth0".to_owned(),
            handle,
            eth0_ip,
        )));

        let gateway = IpAddress::Ipv4(GATEWAY.parse().expect("Invalid gateway address"));
        router.add_ipv4_addr(Ipv4AddrEntry {
            dev: eth0_dev,
            addr: eth0_ip,
            scope: ROUTE_SCOPE_UNIVERSE,
        });
        router.add_route_rule(Rule::with_route_attrs(
            Ipv4Cidr::new(eth0_ip.address(), 32).into(),
            None,
            lo_dev,
            eth0_ip.address().into(),
            65_536,
            RouteAttrs {
                table: ROUTE_TABLE_MAIN,
                protocol: ROUTE_PROTOCOL_BOOT,
                scope: ROUTE_SCOPE_HOST,
                route_type: ROUTE_TYPE_UNICAST,
                prefsrc: Some(eth0_ip.address().into()),
            },
        ));
        router.add_route_rule(Rule::with_route_attrs(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(gateway),
            eth0_dev,
            eth0_ip.address().into(),
            STANDARD_MTU,
            RouteAttrs {
                table: ROUTE_TABLE_MAIN,
                protocol: ROUTE_PROTOCOL_BOOT,
                scope: ROUTE_SCOPE_UNIVERSE,
                route_type: ROUTE_TYPE_UNICAST,
                prefsrc: Some(eth0_ip.address().into()),
            },
        ));

        info!("eth0:");
        info!("  mac:  {}", eth0_address);
        info!("  ip:   {}", eth0_ip);
        subscribe_network_unregister(device_id);
    } else {
        warn!("  No network device found!");
    };

    for dev in &router.devices {
        info!("Device: {}", dev.name());
    }

    SERVICE.init_once(Mutex::new(Service::new(router)));

    SOCKET_SET.init_once(SocketSetWrapper::new());
    LISTEN_TABLE.init_once(ListenTable::new());
    udp::init_udp_registry();

    if let Some(irq) = eth0_irq {
        EthernetDevice::spawn_rx_task(irq);
    }
}

fn subscribe_network_unregister(id: kdevice::DeviceId) {
    subscribe_device_removed(Arc::new(move |removed_id| {
        if removed_id != id || !SERVICE.is_inited() {
            return;
        }
        if SERVICE.lock().remove_device_by_model_id(id) {
            warn!("network: detached removed device {:?}", id);
        }
    }));
}

/// Init vsock subsystem by vsock devices.
#[cfg(feature = "vsock")]
pub fn init_vsock() {
    info!("Initialize vsock subsystem...");
    let mut vsock_devs = vsock_devices();
    if let Some(handle) = vsock_devs.pop() {
        register_vsock_handle(handle);
    } else {
        warn!("  No vsock device found!");
    }
    subscribe_vsock_available(Arc::new(register_vsock_handle));
}

/// Starts vsock runtime workers that require all scheduler run queues.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn start_vsock_bridge() {
    crate::vsock::bridge::start();
}

#[cfg(feature = "vsock")]
fn register_vsock_handle(handle: ClassDevice<VsockDeviceImpl>) {
    use crate::device::{register_vsock_dev, unregister_vsock_dev};

    let id = handle.id();
    info!(
        "  use vsock 0: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );
    if let Err(e) = register_vsock_dev(handle.clone()) {
        warn!("Failed to initialize vsock device: {:?}", e);
    } else {
        #[cfg(feature = "vsock_tipc_bridge")]
        if let Err(e) = crate::vsock::bridge::init(handle.clone()) {
            warn!("Failed to initialize vsock-TIPC bridge: {:?}", e);
        }
        subscribe_device_removed(Arc::new(move |removed_id| {
            if removed_id == id && unregister_vsock_dev(id) {
                warn!("vsock: detached removed device {:?}", id);
            }
        }));
    }
}

pub fn poll_interfaces() {
    while SERVICE.lock().poll(&mut SOCKET_SET.inner.lock()) {}
}
