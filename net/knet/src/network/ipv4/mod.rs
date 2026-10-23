// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native IPv4 packet parsing, construction, and fragmentation.

mod header;
mod process;

pub(crate) use header::{Ipv4Error, Ipv4Header};
pub(crate) use process::{
    Icmpv4Error, Ipv4FragmentError, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP,
    build_icmpv4_error_packet, fragment_output_packet, ip_version, parse_input, payload,
    rewrite_reassembled_header, write_ipv4_packet_header,
};
#[cfg(unittest)]
pub(crate) use process::{build_ipv4_packet, build_ipv4_packet_dont_fragment};
