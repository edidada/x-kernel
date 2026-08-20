// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 packet, input, output, ICMPv6, and NDP helpers.

pub(crate) mod icmpv6;
pub(crate) mod input;
pub(crate) mod ndp;
pub(crate) mod output;
pub(crate) mod packet;
#[cfg(unittest)]
mod tests;

pub(crate) use input::process_ipv6_input;
pub(crate) use output::{Ipv6Action, apply_actions, dispatch_ipv6_packet, queue_ipv6_packet};
#[cfg(unittest)]
pub(crate) use packet::build_ipv6_packet;
pub(crate) use packet::{
    IPV6_DEFAULT_HOP_LIMIT, IPV6_HEADER_LEN, Ipv6Header, PROTOCOL_ICMPV6, PROTOCOL_UDP, payload,
    write_ipv6_packet_header,
};
