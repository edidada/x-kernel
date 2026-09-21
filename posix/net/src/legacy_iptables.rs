// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Userspace boundary for legacy IPv4 iptables queries.

use core::mem::align_of;

use kerrno::{KError, KResult, LinuxError};
use linux_raw_sys::net::socklen_t;
use osvm::VirtPtr;
use posix_types::UserPtr;

pub(super) const IPT_SO_GET_INFO: u32 = 64;
pub(super) const IPT_SO_GET_ENTRIES: u32 = 65;

const XT_TABLE_MAXNAMELEN: usize = 32;
const NF_INET_NUMHOOKS: usize = 5;
const IPT_GETINFO_SIZE: usize = 84;
const IPT_GET_ENTRIES_DATA_OFFSET: usize = 36;
const IPT_GET_ENTRIES_SIZE: usize = align_up(IPT_GET_ENTRIES_DATA_OFFSET, align_of::<u64>());

const GETINFO_VALID_HOOKS_OFFSET: usize = 32;
const GETINFO_HOOK_ENTRY_OFFSET: usize = 36;
const GETINFO_UNDERFLOW_OFFSET: usize = 56;
const GETINFO_NUM_ENTRIES_OFFSET: usize = 76;
const GETINFO_SIZE_OFFSET: usize = 80;

pub(super) const fn is_get_option(optname: u32) -> bool {
    matches!(optname, IPT_SO_GET_INFO | IPT_SO_GET_ENTRIES)
}

pub(super) fn get(optname: u32, optval: UserPtr<u8>, optlen: socklen_t) -> KResult<()> {
    if !kprocess::current_cred().is_privileged() {
        return Err(KError::from(LinuxError::EPERM));
    }

    match optname {
        IPT_SO_GET_INFO => get_info(optval, optlen),
        IPT_SO_GET_ENTRIES => get_entries(optval, optlen),
        _ => Err(KError::from(LinuxError::ENOPROTOOPT)),
    }
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
    output[..6].copy_from_slice(b"filter");
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
    let requested_size = read_u32(&header, XT_TABLE_MAXNAMELEN) as usize;
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
    if &name[..end] != b"filter" {
        return Err(KError::from(LinuxError::ENOENT));
    }
    Ok(())
}

fn write_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32_array(output: &mut [u8], offset: usize, values: &[u32; NF_INET_NUMHOOKS]) {
    for (index, value) in values.iter().enumerate() {
        write_u32(output, offset + index * size_of::<u32>(), *value);
    }
}

fn read_u32(input: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(input[offset..offset + 4].try_into().unwrap())
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
        assert_eq!(IPT_GETINFO_SIZE, 84);
        assert_eq!(IPT_GET_ENTRIES_DATA_OFFSET, 36);
        assert_eq!(
            IPT_GET_ENTRIES_SIZE,
            if align_of::<u64>() == 8 { 40 } else { 36 }
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
        assert_eq!(read_u32(&info, GETINFO_VALID_HOOKS_OFFSET), 0x0e);
        assert_eq!(read_u32(&info, GETINFO_HOOK_ENTRY_OFFSET + 4), 0);
        assert_eq!(read_u32(&info, GETINFO_HOOK_ENTRY_OFFSET + 8), 152);
        assert_eq!(read_u32(&info, GETINFO_HOOK_ENTRY_OFFSET + 12), 304);
        assert_eq!(read_u32(&info, GETINFO_NUM_ENTRIES_OFFSET), 4);
        assert_eq!(read_u32(&info, GETINFO_SIZE_OFFSET), 632);
    }
}
