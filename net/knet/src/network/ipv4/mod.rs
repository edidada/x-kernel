// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native IPv4 validation, construction and fragmentation using etherparse headers.

mod input;
mod output;
mod process;

pub(crate) use input::{Ipv4Error, normalize_packet, parse_icmp_quote, parse_input};
pub(crate) use output::{
    Ipv4FragmentError, fragment_output_packet, prepare_output_header, prepare_output_packet,
    rewrite_reassembled_header, write_ipv4_packet_header,
};
pub(crate) use process::{
    Icmpv4Error, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP, build_icmpv4_error_packet, ip_version,
    payload,
};
#[cfg(unittest)]
pub(crate) use process::{build_ipv4_packet, build_ipv4_packet_dont_fragment};
