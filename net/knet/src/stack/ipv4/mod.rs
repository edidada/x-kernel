// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 packet, input, output, and fragmentation helpers.

pub(crate) mod fragment;
pub(crate) mod input;
pub(crate) mod output;
pub(crate) mod packet;
#[cfg(unittest)]
mod tests;

pub(crate) use input::{process_ipv4_input, queue_icmpv4_error};
pub(crate) use output::{dispatch_ipv4_packet, output_packet_count, queue_ipv4_packet};
pub(crate) use packet::{
    Icmpv4Error, Ipv4Header, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP, ip_version, payload,
    rewrite_reassembled_header, write_ipv4_packet_header,
};
#[cfg(unittest)]
pub(crate) use packet::{
    build_icmpv4_error_packet, build_ipv4_packet, build_ipv4_packet_dont_fragment,
};
