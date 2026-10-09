// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native 64-bit Linux legacy IPv4 entry layouts.
//!
//! Mirrors Linux 6.8 `ip_tables.h`, `x_tables.h`, and `xt_tcpudp.h`.
//! Scalar fields use native byte order; IPv4 addresses remain network-order
//! octets. Explicit padding keeps serialized bytes initialized. Copied reads
//! accept unaligned buffers; the decoder separately validates record alignment.

use core::mem::{align_of, offset_of, size_of};

use linux_raw_sys::net as linux;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::LegacyReplaceError;

pub(super) const NF_INET_NUMHOOKS: usize = linux::nf_inet_hooks::NF_INET_NUMHOOKS as usize;
pub(super) const NF_INET_LOCAL_IN: usize = linux::nf_inet_hooks::NF_INET_LOCAL_IN as usize;
pub(super) const NF_INET_FORWARD: usize = linux::nf_inet_hooks::NF_INET_FORWARD as usize;
pub(super) const NF_INET_LOCAL_OUT: usize = linux::nf_inet_hooks::NF_INET_LOCAL_OUT as usize;
pub(super) const FILTER_VALID_HOOKS: u32 =
    (1 << NF_INET_LOCAL_IN) | (1 << NF_INET_FORWARD) | (1 << NF_INET_LOCAL_OUT);

pub(super) const IPV4_ADDRESS_SIZE: usize = size_of::<u32>();
pub(super) const IFNAMSIZ: usize = linux::IFNAMSIZ as usize;
const XT_EXTENSION_MAXNAMELEN: usize = linux::XT_EXTENSION_MAXNAMELEN as usize;
const XT_FUNCTION_MAXNAMELEN: usize = linux::XT_FUNCTION_MAXNAMELEN as usize;
pub(super) const XT_ERROR_TARGET: &[u8] = linux::XT_ERROR_TARGET;

pub(super) const IPT_INV_VIA_IN: u8 = 0x01;
pub(super) const IPT_INV_VIA_OUT: u8 = 0x02;
pub(super) const IPT_INV_SRCIP: u8 = 0x08;
pub(super) const IPT_INV_DSTIP: u8 = 0x10;
pub(super) const IPT_INV_PROTO: u8 = 0x40;
pub(super) const XT_TCP_INV_SRCPT: u8 = linux::XT_TCP_INV_SRCPT as u8;
pub(super) const XT_TCP_INV_DSTPT: u8 = linux::XT_TCP_INV_DSTPT as u8;
pub(super) const XT_TCP_INV_FLAGS: u8 = linux::XT_TCP_INV_FLAGS as u8;
pub(super) const XT_UDP_INV_SRCPT: u8 = linux::XT_UDP_INV_SRCPT as u8;
pub(super) const XT_UDP_INV_DSTPT: u8 = linux::XT_UDP_INV_DSTPT as u8;
pub(super) const IPT_ICMP_INV: u8 = 0x01;

// Linux standard targets encode terminal verdicts as -NF_* - 1.
pub(super) const XT_VERDICT_DROP: i32 = -(linux::NF_DROP as i32) - 1;
pub(super) const XT_VERDICT_ACCEPT: i32 = -(linux::NF_ACCEPT as i32) - 1;
pub(super) const XT_RETURN: i32 = linux::XT_RETURN;

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct IptIp {
    pub(super) source: [u8; IPV4_ADDRESS_SIZE],
    pub(super) destination: [u8; IPV4_ADDRESS_SIZE],
    pub(super) source_mask: [u8; IPV4_ADDRESS_SIZE],
    pub(super) destination_mask: [u8; IPV4_ADDRESS_SIZE],
    pub(super) input_interface: [u8; IFNAMSIZ],
    pub(super) output_interface: [u8; IFNAMSIZ],
    pub(super) input_interface_mask: [u8; IFNAMSIZ],
    pub(super) output_interface_mask: [u8; IFNAMSIZ],
    pub(super) protocol: u16,
    pub(super) flags: u8,
    pub(super) inverse_flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtCounters {
    pub(super) packets: u64,
    pub(super) bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct IptEntry {
    pub(super) ip: IptIp,
    pub(super) nfcache: u32,
    pub(super) target_offset: u16,
    pub(super) next_offset: u16,
    pub(super) comefrom: u32,
    pub(super) counters: XtCounters,
}

// The userspace member of the native match/target union occupies 32 bytes;
// its kernel pointer member gives that union 8-byte alignment.
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtEntryHeader {
    pub(super) size: u16,
    pub(super) name: [u8; XT_EXTENSION_MAXNAMELEN],
    pub(super) revision: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtStandardTarget {
    pub(super) header: XtEntryHeader,
    pub(super) verdict: i32,
    pub(super) padding: [u8; XT_STANDARD_TARGET_PADDING],
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtErrorTarget {
    pub(super) header: XtEntryHeader,
    pub(super) name: [u8; XT_FUNCTION_MAXNAMELEN],
    pub(super) padding: [u8; XT_ERROR_TARGET_PADDING],
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtTcp {
    pub(super) source_ports: [u16; 2],
    pub(super) destination_ports: [u16; 2],
    pub(super) option: u8,
    pub(super) flags_mask: u8,
    pub(super) flags_compare: u8,
    pub(super) inverse_flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtUdp {
    pub(super) source_ports: [u16; 2],
    pub(super) destination_ports: [u16; 2],
    pub(super) inverse_flags: u8,
    pub(super) padding: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct IptIcmp {
    pub(super) icmp_type: u8,
    pub(super) code: [u8; 2],
    pub(super) inverse_flags: u8,
}

pub(super) const XT_ALIGN: usize = align_of::<IptEntry>();
const XT_STANDARD_TARGET_PADDING: usize =
    size_of::<linux::xt_standard_target>() - size_of::<XtEntryHeader>() - size_of::<i32>();
const XT_ERROR_TARGET_PADDING: usize =
    size_of::<linux::xt_error_target>() - size_of::<XtEntryHeader>() - XT_FUNCTION_MAXNAMELEN;
pub(super) const IPT_ENTRY_SIZE: usize = size_of::<IptEntry>();
pub(super) const IPT_ENTRY_COUNTERS_OFFSET: usize = offset_of!(IptEntry, counters);
pub(super) const XT_ENTRY_HEADER_SIZE: usize = size_of::<XtEntryHeader>();
pub(super) const XT_STANDARD_TARGET_SIZE: usize = size_of::<XtStandardTarget>();
pub(super) const XT_ERROR_TARGET_SIZE: usize = size_of::<XtErrorTarget>();
pub(super) const XT_TCP_MATCH_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<XtTcp>());
pub(super) const XT_UDP_MATCH_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<XtUdp>());
pub(super) const IPT_ICMP_MATCH_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<IptIcmp>());
pub(super) const STANDARD_ENTRY_SIZE: usize = IPT_ENTRY_SIZE + XT_STANDARD_TARGET_SIZE;
pub(super) const ERROR_ENTRY_SIZE: usize = IPT_ENTRY_SIZE + XT_ERROR_TARGET_SIZE;

pub(super) const fn align_up(size: usize) -> usize {
    (size + XT_ALIGN - 1) & !(XT_ALIGN - 1)
}

pub(super) fn read<T: FromBytes>(input: &[u8]) -> Result<T, LegacyReplaceError> {
    T::read_from_prefix(input)
        .map(|(header, _)| header)
        .map_err(|_| LegacyReplaceError::InvalidInput)
}

pub(super) fn write<T: IntoBytes + Immutable>(output: &mut [u8], header: &T) {
    header
        .write_to_prefix(output)
        .expect("validated legacy record must contain its fixed header");
}

const _: () = {
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<IptIp>() == 84);
    assert!(align_of::<IptIp>() == 4);
    assert!(offset_of!(IptIp, protocol) == 80);
    assert!(offset_of!(IptIp, flags) == 82);
    assert!(offset_of!(IptIp, inverse_flags) == 83);
    assert!(size_of::<XtCounters>() == 16);
    assert!(IPT_ENTRY_SIZE == 112);
    assert!(XT_ALIGN == 8);
    assert!(offset_of!(IptEntry, target_offset) == 88);
    assert!(offset_of!(IptEntry, next_offset) == 90);
    assert!(IPT_ENTRY_COUNTERS_OFFSET == 96);
    assert!(XT_ENTRY_HEADER_SIZE == 32);
    assert!(align_of::<XtEntryHeader>() == XT_ALIGN);
    assert!(offset_of!(XtEntryHeader, name) == 2);
    assert!(offset_of!(XtEntryHeader, revision) == 31);
    assert!(XT_STANDARD_TARGET_SIZE == 40);
    assert!(offset_of!(XtStandardTarget, verdict) == 32);
    assert!(XT_ERROR_TARGET_SIZE == 64);
    assert!(size_of::<XtTcp>() == 12);
    assert!(size_of::<XtUdp>() == 10);
    assert!(size_of::<IptIcmp>() == 4);
    assert!(XT_TCP_MATCH_SIZE == 48);
    assert!(XT_UDP_MATCH_SIZE == 48);
    assert!(IPT_ICMP_MATCH_SIZE == 40);
};
