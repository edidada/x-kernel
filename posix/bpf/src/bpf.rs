// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc};
use core::{ffi::c_char, mem::size_of};

use bpffs::BpfProgram;
use kerrno::{KError, KResult};
use klogger::debug;
use kuaccess::vm_load_string;
use kvfs::path::Path;
use posix_types::{UserConstPtr, UserRead};

/// `BPF_PROG_LOAD` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_PROG_LOAD: u32 = 5;
/// `BPF_OBJ_PIN` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_OBJ_PIN: u32 = 6;
/// `BPF_OBJ_GET` (`enum bpf_cmd` in Linux `uapi/linux/bpf.h`).
const BPF_OBJ_GET: u32 = 7;

/// Minimum `size` for `BPF_PROG_LOAD` covering fields through `prog_flags`
/// (Linux `union bpf_attr` prog_load prefix).
const BPF_PROG_LOAD_MIN_ATTR_SIZE: usize = size_of::<BpfProgLoadAttrPrefix>();
/// Maximum instruction count accepted by the current unprivileged loader.
///
/// This follows Linux's classic `BPF_MAXINSNS` limit. X-Kernel does not yet
/// have the verifier and capability split needed to safely expose Linux's
/// larger privileged-program limit.
const BPF_MAX_INSNS: u32 = 4096;

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfProgLoadAttrPrefix {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
}

const _: () = assert!(size_of::<BpfProgLoadAttrPrefix>() == 48);

/// Minimum `size` for `BPF_OBJ_ATTR` covering fields through `file_flags`
/// (Linux `union bpf_attr` obj_attr prefix).
const BPF_OBJ_ATTR_MIN_SIZE: usize = size_of::<BpfObjAttrPrefix>();

#[repr(C)]
#[derive(Clone, Copy, UserRead)]
struct BpfObjAttrPrefix {
    pathname: u64,
    bpf_fd: u32,
    file_flags: u32,
}

const _: () = assert!(size_of::<BpfObjAttrPrefix>() == 16);

/// Linux `bpf(cmd, attr, size)` — see `man 2 bpf`.
///
/// Program loads are limited to [`BPF_MAX_INSNS`] instructions before any
/// user-memory allocation or copy is attempted.
pub fn sys_bpf(cmd: i32, attr_ptr: usize, size: u32) -> KResult<isize> {
    if cmd < 0 {
        return Err(KError::InvalidInput);
    }
    let cmd_u = cmd as u32;
    match cmd_u {
        BPF_PROG_LOAD => bpf_prog_load(attr_ptr, size),
        BPF_OBJ_PIN => bpf_obj_pin(attr_ptr, size),
        BPF_OBJ_GET => bpf_obj_get(attr_ptr, size),
        _ => {
            debug!("sys_bpf: cmd {cmd} not implemented");
            Err(KError::OperationNotSupported)
        }
    }
}

fn bpf_prog_load(attr_ptr: usize, size: u32) -> KResult<isize> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_PROG_LOAD_MIN_ATTR_SIZE {
        return Err(KError::InvalidInput);
    }

    let prefix = UserConstPtr::<BpfProgLoadAttrPrefix>::from(attr_ptr).read_vm()?;
    let prog_bytes = checked_program_size(prefix.insn_cnt)?;

    let insns = UserConstPtr::<u8>::from(prefix.insns as usize).load_vm_vec(prog_bytes)?;

    let arc_insns: Arc<[u8]> = insns.into_boxed_slice().into();
    let fd =
        kthread::current_resources().add_file_like(Arc::new(BpfProgram::new(arc_insns)), true)?;
    Ok(fd as isize)
}

fn checked_program_size(insn_cnt: u32) -> KResult<usize> {
    if insn_cnt == 0 || insn_cnt > BPF_MAX_INSNS {
        return Err(KError::InvalidInput);
    }

    (insn_cnt as usize)
        .checked_mul(size_of::<u64>())
        .ok_or(KError::InvalidInput)
}

fn bpf_obj_pin(attr_ptr: usize, size: u32) -> KResult<isize> {
    let attr = read_obj_attr(attr_ptr, size)?;
    if attr.file_flags != 0 {
        return Err(KError::InvalidInput);
    }
    let pathname = load_pathname(attr.pathname)?;
    let program = kthread::current_resources().get_file_like_as::<BpfProgram>(attr.bpf_fd as _)?;

    let fs_context = kthread::current_fs_context();
    let fs = fs_context.lock();
    let (parent, name) = fs.resolve_parent(Path::new(&pathname))?;
    bpffs::pin_program(&parent, name.as_ref(), program)?;
    Ok(0)
}

fn bpf_obj_get(attr_ptr: usize, size: u32) -> KResult<isize> {
    let attr = read_obj_attr(attr_ptr, size)?;
    if attr.file_flags != 0 {
        return Err(KError::InvalidInput);
    }
    let pathname = load_pathname(attr.pathname)?;

    let fs_context = kthread::current_fs_context();
    let fs = fs_context.lock();
    let location = fs.resolve(Path::new(&pathname))?;
    let program = bpffs::program_from_location(&location)?;
    let fd = kthread::current_resources().add_file_like(program, true)?;
    Ok(fd as isize)
}

fn read_obj_attr(attr_ptr: usize, size: u32) -> KResult<BpfObjAttrPrefix> {
    if attr_ptr == 0 {
        return Err(KError::InvalidInput);
    }
    if (size as usize) < BPF_OBJ_ATTR_MIN_SIZE {
        return Err(KError::InvalidInput);
    }

    Ok(UserConstPtr::<BpfObjAttrPrefix>::from(attr_ptr).read_vm()?)
}

fn load_pathname(pathname: u64) -> KResult<String> {
    if pathname == 0 {
        return Err(KError::InvalidInput);
    }
    vm_load_string(pathname as *const c_char)
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::{BPF_MAX_INSNS, checked_program_size};

    #[def_test]
    fn program_size_enforces_instruction_bounds() {
        assert!(checked_program_size(0).is_err());
        assert_eq!(checked_program_size(1).unwrap(), 8);
        assert_eq!(
            checked_program_size(BPF_MAX_INSNS).unwrap(),
            BPF_MAX_INSNS as usize * 8
        );
        assert!(checked_program_size(BPF_MAX_INSNS + 1).is_err());
    }
}
