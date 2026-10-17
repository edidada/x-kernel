// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stubbed `bpf(2)` frontend for the cgroup device controller.
//!
//! The kernel runs no eBPF interpreter, but non-rootless `runc` programs the
//! cgroup-v2 device controller unconditionally: it probes feature support,
//! creates a map, loads `BPF_PROG_TYPE_CGROUP_DEVICE` programs, attaches them
//! to the cgroup directory, and queries the cgroup for attached programs (see
//! `libcontainer/cgroups/devices` in runc). Refusing those calls makes every
//! non-rootless container start fail.
//!
//! Because the kernel enforces no device access control, the controller is
//! modelled as a permissive no-op: map creation and program loading return a
//! real placeholder fd ([`kfd_objects::bpf_stub`]), attach/detach validate the
//! referenced fds (user-space feature probes rely on `EBADF` classification)
//! and otherwise succeed, and queries report zero attached programs. Every
//! other `bpf(2)` command keeps the `ENOSYS` behavior user space already
//! probes for.

use kerrno::{KError, KResult};
use kfd_objects::bpf_stub::BpfProgramPlaceholder;
use posix_types::{UserConstPtr, UserPtr};

/// `BPF_MAP_CREATE` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`); the device
/// filter program is loaded together with its map.
const BPF_MAP_CREATE: u32 = 0;
/// `BPF_PROG_LOAD` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_PROG_LOAD: u32 = 5;
/// `BPF_PROG_ATTACH` (`enum bpf_cmd`).
const BPF_PROG_ATTACH: u32 = 8;
/// `BPF_PROG_DETACH` (`enum bpf_cmd`).
const BPF_PROG_DETACH: u32 = 9;
/// `BPF_PROG_GET_NEXT_ID` (`enum bpf_cmd`).
const BPF_PROG_GET_NEXT_ID: u32 = 11;
/// `BPF_PROG_GET_FD_BY_ID` (`enum bpf_cmd`).
const BPF_PROG_GET_FD_BY_ID: u32 = 13;
/// `BPF_PROG_QUERY` (`enum bpf_cmd`).
const BPF_PROG_QUERY: u32 = 16;
/// `BPF_LINK_CREATE` (`enum bpf_cmd`), used by newer runc releases.
const BPF_LINK_CREATE: u32 = 28;

/// Offset of `target_fd` in the attach/link/query attribute layouts.
const ATTR_TARGET_FD_OFFSET: usize = 0;
/// Offset of `attach_bpf_fd` in `union bpf_attr::prog_attach`.
const ATTR_ATTACH_BPF_FD_OFFSET: usize = 4;
/// Offset of `prog_cnt` in `union bpf_attr::prog_query`.
const ATTR_QUERY_PROG_CNT_OFFSET: usize = 16;
/// Offset of `prog_cnt` in the original `prog_query` layout where
/// `prog_ids` preceded it (still used by runc 1.1.5).
const ATTR_QUERY_PROG_CNT_LEGACY_OFFSET: usize = 24;
/// Minimum `union bpf_attr` size covering fields through `attach_bpf_fd`.
const MIN_ATTACH_ATTR_SIZE: u32 = 8;
/// Minimum `union bpf_attr` size covering `prog_cnt` in either layout.
const MIN_QUERY_ATTR_SIZE: u32 = 28;
/// Minimum `union bpf_attr` size covering fields through `prog_type`.
const MIN_PROG_ATTR_SIZE: u32 = 4;
/// Minimum `union bpf_attr` size covering the `start_id`/`prog_id` field.
const MIN_ID_ATTR_SIZE: u32 = 4;

/// Handles the subset of `bpf(2)` commands the stubbed device controller
/// needs; see the [module documentation](self).
pub(crate) fn sys_bpf_stub(cmd: u32, attr_addr: usize, attr_size: u32) -> KResult<isize> {
    warn!("bpf stub <= cmd: {cmd}, attr_size: {attr_size}");
    match cmd {
        BPF_MAP_CREATE | BPF_PROG_LOAD | BPF_LINK_CREATE => {
            if attr_size < MIN_PROG_ATTR_SIZE {
                return Err(KError::InvalidInput);
            }
            let file = BpfProgramPlaceholder::new_file(kprocess::current_cred())?;
            kprocess::current_files()
                .add_file(file, true)
                .map(|fd| fd as _)
        }
        BPF_PROG_ATTACH | BPF_PROG_DETACH => {
            if attr_size < MIN_ATTACH_ATTR_SIZE {
                return Err(KError::InvalidInput);
            }
            // Feature probes classify support by the errno of an attach on
            // deliberately invalid fds: report `EBADF` for missing fds like
            // Linux, and treat attach_type as a valid request unconditionally
            // (the no-op controller accepts any attach type).
            check_fd_at(attr_addr + ATTR_TARGET_FD_OFFSET)?;
            check_fd_at(attr_addr + ATTR_ATTACH_BPF_FD_OFFSET)?;
            Ok(0)
        }
        BPF_PROG_QUERY => {
            check_fd_at(attr_addr + ATTR_TARGET_FD_OFFSET)?;
            // Report "no programs attached": the permissive no-op controller
            // has nothing to list, so callers conclude the cgroup is clean.
            // The kernel's output `prog_cnt` field sits at offset 16 in the
            // current uapi layout but at offset 24 in the original layout
            // (where `prog_ids` came first, still used by runc 1.1.5), so
            // both are zeroed to satisfy either caller.
            if attr_size < MIN_QUERY_ATTR_SIZE {
                return Err(KError::InvalidInput);
            }
            UserPtr::<u32>::from(attr_addr + ATTR_QUERY_PROG_CNT_OFFSET).write_vm(0)?;
            UserPtr::<u32>::from(attr_addr + ATTR_QUERY_PROG_CNT_LEGACY_OFFSET).write_vm(0)?;
            Ok(0)
        }
        // Program iteration reports an empty ID space: with no attached
        // programs there is no `next_id` after any start, which is how Linux
        // terminates a `BPF_PROG_GET_NEXT_ID` scan (`ENOENT`).
        BPF_PROG_GET_NEXT_ID | BPF_PROG_GET_FD_BY_ID => {
            if attr_size < MIN_ID_ATTR_SIZE {
                return Err(KError::InvalidInput);
            }
            Err(KError::NotFound)
        }
        _ => Err(KError::Unsupported),
    }
}

/// Returns `EBADF` when the caller has no fd at the given attribute offset.
fn check_fd_at(attr_addr: usize) -> KResult<()> {
    let fd = UserConstPtr::<i32>::from(attr_addr).read_vm()?;
    kprocess::current_files().get_file(fd)?;
    Ok(())
}
