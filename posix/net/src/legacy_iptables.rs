// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace boundary for native legacy IPv4 iptables table operations.
//! Header sizes and field offsets follow C layouts and Linux UAPI bindings.

use core::mem::{align_of, offset_of, size_of};

use kerrno::{KError, KResult, LinuxError};
use linux_raw_sys::net::{self as linux, socklen_t};
use osvm::VirtPtr;
use posix_types::{UserConstPtr, UserPtr};

const IPT_BASE_CTL: u32 = 64;
pub(super) const IPT_SO_SET_REPLACE: u32 = IPT_BASE_CTL;
pub(super) const IPT_SO_SET_ADD_COUNTERS: u32 = IPT_BASE_CTL + 1;
pub(super) const IPT_SO_GET_INFO: u32 = IPT_BASE_CTL;
pub(super) const IPT_SO_GET_ENTRIES: u32 = IPT_BASE_CTL + 1;
pub(super) const IPT_SO_GET_REVISION_MATCH: u32 = IPT_BASE_CTL + 2;
pub(super) const IPT_SO_GET_REVISION_TARGET: u32 = IPT_BASE_CTL + 3;

const FILTER_TABLE_NAME: &[u8] = b"filter";
const XT_TABLE_MAXNAMELEN: usize = linux::XT_TABLE_MAXNAMELEN as usize;
const NF_INET_NUMHOOKS: usize = linux::nf_inet_hooks::NF_INET_NUMHOOKS as usize;

// IPv4-specific headers are absent from linux-raw-sys; flexible entry arrays
// are excluded here and their starting offset follows x_tables alignment.
#[repr(C)]
struct IptGetInfo {
    name: [u8; XT_TABLE_MAXNAMELEN],
    valid_hooks: u32,
    hook_entry: [u32; NF_INET_NUMHOOKS],
    underflow: [u32; NF_INET_NUMHOOKS],
    num_entries: u32,
    size: u32,
}

#[repr(C)]
struct IptGetEntriesHeader {
    name: [u8; XT_TABLE_MAXNAMELEN],
    size: u32,
}

#[repr(C)]
struct IptReplace {
    name: [u8; XT_TABLE_MAXNAMELEN],
    valid_hooks: u32,
    num_entries: u32,
    size: u32,
    hook_entry: [u32; NF_INET_NUMHOOKS],
    underflow: [u32; NF_INET_NUMHOOKS],
    num_counters: u32,
    counters: *mut linux::xt_counters,
}

const IPT_GETINFO_SIZE: usize = size_of::<IptGetInfo>();
const IPT_GET_ENTRIES_DATA_OFFSET: usize = align_up(
    size_of::<IptGetEntriesHeader>(),
    align_of::<linux::_xt_align>(),
);
const IPT_GET_ENTRIES_SIZE: usize = IPT_GET_ENTRIES_DATA_OFFSET;
const GET_ENTRIES_SIZE_OFFSET: usize = offset_of!(IptGetEntriesHeader, size);

const GETINFO_VALID_HOOKS_OFFSET: usize = offset_of!(IptGetInfo, valid_hooks);
const GETINFO_HOOK_ENTRY_OFFSET: usize = offset_of!(IptGetInfo, hook_entry);
const GETINFO_UNDERFLOW_OFFSET: usize = offset_of!(IptGetInfo, underflow);
const GETINFO_NUM_ENTRIES_OFFSET: usize = offset_of!(IptGetInfo, num_entries);
const GETINFO_SIZE_OFFSET: usize = offset_of!(IptGetInfo, size);
const IPT_REPLACE_HEADER_SIZE: usize = size_of::<IptReplace>();
const IPT_REPLACE_VALID_HOOKS_OFFSET: usize = offset_of!(IptReplace, valid_hooks);
const IPT_REPLACE_NUM_ENTRIES_OFFSET: usize = offset_of!(IptReplace, num_entries);
const IPT_REPLACE_SIZE_OFFSET: usize = offset_of!(IptReplace, size);
const IPT_REPLACE_HOOK_ENTRY_OFFSET: usize = offset_of!(IptReplace, hook_entry);
const IPT_REPLACE_UNDERFLOW_OFFSET: usize = offset_of!(IptReplace, underflow);
const IPT_REPLACE_NUM_COUNTERS_OFFSET: usize = offset_of!(IptReplace, num_counters);
const IPT_REPLACE_COUNTERS_OFFSET: usize = offset_of!(IptReplace, counters);
const XT_COUNTERS_INFO_SIZE: usize = size_of::<linux::xt_counters_info>();
const XT_COUNTERS_INFO_COUNT_OFFSET: usize = offset_of!(linux::xt_counters_info, num_counters);
const XT_COUNTER_SIZE: usize = size_of::<linux::xt_counters>();
const XT_COUNTER_PACKETS_OFFSET: usize = offset_of!(linux::xt_counters, pcnt);
const XT_COUNTER_BYTES_OFFSET: usize = offset_of!(linux::xt_counters, bcnt);
const XT_GET_REVISION_SIZE: usize = size_of::<linux::xt_get_revision>();
const XT_GET_REVISION_OFFSET: usize = offset_of!(linux::xt_get_revision, revision);
const XT_EXTENSION_MAXNAMELEN: usize = linux::XT_EXTENSION_MAXNAMELEN as usize;

pub(super) const fn is_set_option(optname: u32) -> bool {
    matches!(optname, IPT_SO_SET_REPLACE | IPT_SO_SET_ADD_COUNTERS)
}

pub(super) fn set(optname: u32, optval: UserConstPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if !kprocess::current_cred().is_privileged() {
        return Err(KError::from(LinuxError::EPERM));
    }
    match optname {
        IPT_SO_SET_REPLACE => replace(optval, optlen),
        IPT_SO_SET_ADD_COUNTERS => add_counters(optval, optlen),
        _ => Err(KError::from(LinuxError::ENOPROTOOPT)),
    }
}

fn replace(optval: UserConstPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if (optlen as usize) < IPT_REPLACE_HEADER_SIZE {
        return Err(KError::InvalidInput);
    }
    let header = osvm::load_vec::<u8>(optval.as_ptr(), IPT_REPLACE_HEADER_SIZE)?;
    require_filter_table_bytes(&header[..XT_TABLE_MAXNAMELEN])?;
    let size = read_u32(&header, IPT_REPLACE_SIZE_OFFSET) as usize;
    let num_entries = read_u32(&header, IPT_REPLACE_NUM_ENTRIES_OFFSET);
    let num_counters = read_u32(&header, IPT_REPLACE_NUM_COUNTERS_OFFSET);
    if size > knet::MAX_LEGACY_FILTER_ENTRIES_SIZE
        || num_entries == 0
        || num_entries as usize > knet::MAX_LEGACY_FILTER_ENTRY_COUNT
        || num_counters == 0
        || num_counters >= (i32::MAX as u32 / XT_COUNTER_SIZE as u32)
        || size
            .checked_add(IPT_REPLACE_HEADER_SIZE)
            .is_none_or(|total| total > optlen as usize)
    {
        return Err(KError::InvalidInput);
    }
    let entries =
        osvm::load_vec::<u8>(optval.as_ptr().wrapping_add(IPT_REPLACE_HEADER_SIZE), size)?;
    let mut hook_entry = [0; NF_INET_NUMHOOKS];
    let mut underflow = [0; NF_INET_NUMHOOKS];
    for index in 0..NF_INET_NUMHOOKS {
        hook_entry[index] = read_u32(
            &header,
            IPT_REPLACE_HOOK_ENTRY_OFFSET + index * size_of::<u32>(),
        );
        underflow[index] = read_u32(
            &header,
            IPT_REPLACE_UNDERFLOW_OFFSET + index * size_of::<u32>(),
        );
    }
    let request = knet::LegacyFilterReplace {
        valid_hooks: read_u32(&header, IPT_REPLACE_VALID_HOOKS_OFFSET),
        hook_entry,
        underflow,
        num_entries,
        num_counters,
        entries: &entries,
    };
    let old_counters =
        knet::replace_legacy_ipv4_filter_table(request).map_err(|error| match error {
            knet::LegacyReplaceError::InvalidInput => KError::InvalidInput,
            knet::LegacyReplaceError::CounterMismatch => KError::from(LinuxError::EAGAIN),
        })?;

    // Linux reports success after publication even if the old counters cannot be copied.
    let counters_ptr = usize::from_ne_bytes(
        header[IPT_REPLACE_COUNTERS_OFFSET..IPT_REPLACE_COUNTERS_OFFSET + size_of::<usize>()]
            .try_into()
            .unwrap(),
    );
    let mut counters = alloc::vec::Vec::with_capacity(old_counters.len() * XT_COUNTER_SIZE);
    for counter in old_counters {
        counters.extend_from_slice(&counter.packets.to_ne_bytes());
        counters.extend_from_slice(&counter.bytes.to_ne_bytes());
    }
    let _ = osvm::write_vm_bytes(counters_ptr as *mut u8, &counters);
    Ok(())
}

fn add_counters(optval: UserConstPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if (optlen as usize) < XT_COUNTERS_INFO_SIZE {
        return Err(KError::InvalidInput);
    }
    let header = osvm::load_vec::<u8>(optval.as_ptr(), XT_COUNTERS_INFO_SIZE)?;
    require_filter_table_bytes(&header[..XT_TABLE_MAXNAMELEN])?;
    let count = read_u32(&header, XT_COUNTERS_INFO_COUNT_OFFSET) as usize;
    if count == 0
        || count > knet::MAX_LEGACY_FILTER_ENTRY_COUNT
        || XT_COUNTERS_INFO_SIZE + count * XT_COUNTER_SIZE != optlen as usize
    {
        return Err(KError::InvalidInput);
    }
    let counters = osvm::load_vec::<u8>(
        optval.as_ptr().wrapping_add(XT_COUNTERS_INFO_SIZE),
        count * XT_COUNTER_SIZE,
    )?;
    let counters = counters
        .chunks_exact(XT_COUNTER_SIZE)
        .map(|record| knet::LegacyCounter {
            packets: read_u64(record, XT_COUNTER_PACKETS_OFFSET),
            bytes: read_u64(record, XT_COUNTER_BYTES_OFFSET),
        })
        .collect::<alloc::vec::Vec<_>>();
    knet::add_legacy_ipv4_filter_counters(&counters).map_err(|error| match error {
        knet::LegacyReplaceError::CounterMismatch => KError::InvalidInput,
        knet::LegacyReplaceError::InvalidInput => KError::InvalidInput,
    })
}

pub(super) const fn is_get_option(optname: u32) -> bool {
    matches!(
        optname,
        IPT_SO_GET_INFO
            | IPT_SO_GET_ENTRIES
            | IPT_SO_GET_REVISION_MATCH
            | IPT_SO_GET_REVISION_TARGET
    )
}

pub(super) fn get(optname: u32, optval: UserPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if !kprocess::current_cred().is_privileged() {
        return Err(KError::from(LinuxError::EPERM));
    }

    match optname {
        IPT_SO_GET_INFO => get_info(optval, optlen),
        IPT_SO_GET_ENTRIES => get_entries(optval, optlen),
        IPT_SO_GET_REVISION_MATCH | IPT_SO_GET_REVISION_TARGET => {
            get_revision(optname, optval, optlen)
        }
        _ => Err(KError::from(LinuxError::ENOPROTOOPT)),
    }
}

fn get_revision(optname: u32, optval: UserPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if optlen as usize != XT_GET_REVISION_SIZE {
        return Err(KError::InvalidInput);
    }
    let mut request = osvm::load_vec::<u8>(optval.as_ptr(), XT_GET_REVISION_SIZE)?;
    request[XT_EXTENSION_MAXNAMELEN - 1] = 0;
    let end = request[..XT_EXTENSION_MAXNAMELEN]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap();
    let name = &request[..end];
    supported_revision(optname, name, request[XT_GET_REVISION_OFFSET])
}

fn supported_revision(optname: u32, name: &[u8], revision: u8) -> KResult<()> {
    let is_supported = if optname == IPT_SO_GET_REVISION_MATCH {
        name == b"tcp" || name == b"udp" || name == b"icmp"
    } else {
        name.is_empty() || name == b"ERROR"
    };
    if !is_supported {
        return Err(KError::from(LinuxError::ENOENT));
    }
    if revision != 0 {
        return Err(KError::from(LinuxError::EPROTONOSUPPORT));
    }
    Ok(())
}

fn get_info(optval: UserPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if optlen as usize != IPT_GETINFO_SIZE {
        return Err(KError::InvalidInput);
    }
    require_filter_table(optval)?;

    let table = knet::legacy_ipv4_filter_table();
    let output = encode_info(&table);
    osvm::write_vm_bytes(optval.as_ptr().cast_mut(), &output)?;
    Ok(())
}

fn encode_info(table: &knet::LegacyFilterTable) -> [u8; IPT_GETINFO_SIZE] {
    let mut output = [0u8; IPT_GETINFO_SIZE];
    output[..FILTER_TABLE_NAME.len()].copy_from_slice(FILTER_TABLE_NAME);
    write_u32(&mut output, GETINFO_VALID_HOOKS_OFFSET, table.valid_hooks());
    write_u32_array(&mut output, GETINFO_HOOK_ENTRY_OFFSET, table.hook_entry());
    write_u32_array(&mut output, GETINFO_UNDERFLOW_OFFSET, table.underflow());
    write_u32(&mut output, GETINFO_NUM_ENTRIES_OFFSET, table.num_entries());
    write_u32(
        &mut output,
        GETINFO_SIZE_OFFSET,
        u32::try_from(table.entries().len()).expect("validated legacy table size must fit u32"),
    );
    output
}

fn get_entries(optval: UserPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if (optlen as usize) < IPT_GET_ENTRIES_SIZE {
        return Err(KError::InvalidInput);
    }
    let header = osvm::load_vec::<u8>(optval.as_ptr(), IPT_GET_ENTRIES_SIZE)?;
    let requested_size = read_u32(&header, GET_ENTRIES_SIZE_OFFSET) as usize;
    if optlen as usize != IPT_GET_ENTRIES_SIZE.saturating_add(requested_size) {
        return Err(KError::InvalidInput);
    }
    require_filter_table_bytes(&header[..XT_TABLE_MAXNAMELEN])?;

    let table = knet::legacy_ipv4_filter_table();
    if requested_size != table.entries().len() {
        return Err(KError::from(LinuxError::EAGAIN));
    }
    osvm::write_vm_bytes(
        optval
            .as_ptr()
            .wrapping_add(IPT_GET_ENTRIES_DATA_OFFSET)
            .cast_mut(),
        table.entries(),
    )?;
    Ok(())
}

fn require_filter_table(optval: UserPtr<u8>) -> KResult<()> {
    let name = osvm::load_vec::<u8>(optval.as_ptr(), XT_TABLE_MAXNAMELEN)?;
    require_filter_table_bytes(&name)
}

fn require_filter_table_bytes(name: &[u8]) -> KResult<()> {
    let mut name = name.to_vec();
    name[XT_TABLE_MAXNAMELEN - 1] = 0;
    let end = name.iter().position(|byte| *byte == 0).unwrap();
    if &name[..end] != FILTER_TABLE_NAME {
        return Err(KError::from(LinuxError::ENOENT));
    }
    Ok(())
}

fn write_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + size_of::<u32>()].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32_array(output: &mut [u8], offset: usize, values: &[u32; NF_INET_NUMHOOKS]) {
    for (index, value) in values.iter().enumerate() {
        write_u32(output, offset + index * size_of::<u32>(), *value);
    }
}

fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(input[offset..offset + size_of::<u32>()].try_into().unwrap())
}

fn read_u64(input: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(input[offset..offset + size_of::<u64>()].try_into().unwrap())
}

const fn align_up(size: usize, alignment: usize) -> usize {
    (size + alignment - 1) & !(alignment - 1)
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn legacy_structure_sizes_match_native_linux_uapi() {
        // These literals are independent Linux ABI expectations, so a layout
        // change cannot silently change both serialization and its oracle.
        assert_eq!(IPT_GETINFO_SIZE, 84);
        assert_eq!(GETINFO_VALID_HOOKS_OFFSET, 32);
        assert_eq!(GETINFO_HOOK_ENTRY_OFFSET, 36);
        assert_eq!(GETINFO_UNDERFLOW_OFFSET, 56);
        assert_eq!(GETINFO_NUM_ENTRIES_OFFSET, 76);
        assert_eq!(GETINFO_SIZE_OFFSET, 80);
        assert_eq!(IPT_GET_ENTRIES_DATA_OFFSET, 40);
        assert_eq!(GET_ENTRIES_SIZE_OFFSET, 32);
        assert_eq!(IPT_REPLACE_HEADER_SIZE, 96);
        assert_eq!(IPT_REPLACE_VALID_HOOKS_OFFSET, 32);
        assert_eq!(IPT_REPLACE_NUM_ENTRIES_OFFSET, 36);
        assert_eq!(IPT_REPLACE_SIZE_OFFSET, 40);
        assert_eq!(IPT_REPLACE_HOOK_ENTRY_OFFSET, 44);
        assert_eq!(IPT_REPLACE_UNDERFLOW_OFFSET, 64);
        assert_eq!(IPT_REPLACE_NUM_COUNTERS_OFFSET, 84);
        assert_eq!(IPT_REPLACE_COUNTERS_OFFSET, 88);
        assert_eq!(XT_COUNTERS_INFO_SIZE, 40);
        assert_eq!(XT_COUNTERS_INFO_COUNT_OFFSET, 32);
        assert_eq!(XT_COUNTER_SIZE, 16);
        assert_eq!(XT_COUNTER_PACKETS_OFFSET, 0);
        assert_eq!(XT_COUNTER_BYTES_OFFSET, 8);
        assert_eq!(XT_GET_REVISION_SIZE, 30);
        assert_eq!(XT_GET_REVISION_OFFSET, 29);
        assert_eq!(
            IPT_GET_ENTRIES_SIZE,
            if align_of::<u64>() == 8 { 40 } else { 36 }
        );
        assert_eq!(is_set_option(IPT_SO_SET_REPLACE), true);
        assert_eq!(is_set_option(IPT_SO_SET_ADD_COUNTERS), true);
        assert_eq!(IPT_SO_SET_REPLACE, 64);
        assert_eq!(IPT_SO_SET_ADD_COUNTERS, 65);
        assert_eq!(IPT_SO_GET_INFO, 64);
        assert_eq!(IPT_SO_GET_ENTRIES, 65);
        assert_eq!(IPT_SO_GET_REVISION_MATCH, 66);
        assert_eq!(IPT_SO_GET_REVISION_TARGET, 67);
    }

    #[def_test]
    fn revision_query_reports_only_supported_revision_zero_extensions() {
        assert_eq!(
            supported_revision(IPT_SO_GET_REVISION_MATCH, b"tcp", 0),
            Ok(())
        );
        assert_eq!(
            supported_revision(IPT_SO_GET_REVISION_MATCH, b"udp", 0),
            Ok(())
        );
        assert_eq!(
            supported_revision(IPT_SO_GET_REVISION_MATCH, b"icmp", 0),
            Ok(())
        );
        assert_eq!(
            supported_revision(IPT_SO_GET_REVISION_TARGET, b"", 0),
            Ok(())
        );
        assert_eq!(
            supported_revision(IPT_SO_GET_REVISION_MATCH, b"tcp", 1),
            Err(KError::from(LinuxError::EPROTONOSUPPORT))
        );
        assert_eq!(
            supported_revision(IPT_SO_GET_REVISION_MATCH, b"conntrack", 0),
            Err(KError::from(LinuxError::ENOENT))
        );
    }

    #[def_test]
    fn table_name_uses_linux_nul_termination_rules() {
        let mut filter = [0u8; XT_TABLE_MAXNAMELEN];
        filter[..6].copy_from_slice(b"filter");
        assert_eq!(require_filter_table_bytes(&filter), Ok(()));

        let mut suffix_after_nul = filter;
        suffix_after_nul[7..11].copy_from_slice(b"junk");
        assert_eq!(require_filter_table_bytes(&suffix_after_nul), Ok(()));

        let missing = [b'x'; XT_TABLE_MAXNAMELEN];
        assert_eq!(
            require_filter_table_bytes(&missing),
            Err(KError::from(LinuxError::ENOENT))
        );
    }

    #[def_test]
    fn get_info_encodes_filter_snapshot_metadata() {
        let table = knet::legacy_ipv4_filter_table();
        let info = encode_info(&table);

        assert_eq!(&info[..7], b"filter\0");
        assert_eq!(read_u32(&info, 32), 0x0e);
        assert_eq!(read_u32(&info, 40), 0);
        assert_eq!(read_u32(&info, 44), 152);
        assert_eq!(read_u32(&info, 48), 304);
        assert_eq!(read_u32(&info, 76), 4);
        assert_eq!(read_u32(&info, 80), 632);
    }
}
