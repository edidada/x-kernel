// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fresh process construction for an already accepted in-memory TA image.

use alloc::{format, sync::Arc, vec, vec::Vec};

use fs_context::copy_init_fs_struct;
use kcred::initial_cred;
use kerrno::{KError, KResult};
use kexec::load_user_elf_image;
use khal::uspace::UserContext;
use kprocess::{Process, UserThreadRuntimeAction, build_process_thread, publish_user_task};
use ksync::Mutex;
use tee_task_iface::ta_ctx::{bytes_to_ta_head, read_ta_head_from_image};

use crate::{new_user_task, prepare_user_entry};

/// Copies an accepted TA image into a fresh process, publishes it, and makes
/// its first thread runnable. The source image is released after loading.
///
/// The caller owns signature policy. This function validates executable layout
/// and TA metadata; it never opens or writes a TA file. A failed launch leaves
/// no runnable task and discards the partially built address space.
///
/// This function requires the global init process to exist already. It must run
/// in sleepable kernel-task context after scheduler and process initialization.
///
/// # Errors
///
/// Returns an error for missing or invalid `.ta_head` metadata, invalid ELF
/// layout, PID allocation failure, process/thread construction failure, or task
/// publication failure.
pub fn spawn_dynamic_ta(
    image: Vec<u8>,
    dispatch_syscall: impl FnMut(&mut UserContext, &kprocess::Thread) -> UserThreadRuntimeAction
    + Send
    + 'static,
) -> KResult {
    let head_bytes = read_ta_head_from_image(&image)?.ok_or(KError::InvalidExecutable)?;
    let head = bytes_to_ta_head(&head_bytes)?;
    let uuid = head.uuid;
    let ta_name = format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}.ta",
        uuid.timeLow,
        uuid.timeMid,
        uuid.timeHiAndVersion,
        uuid.clockSeqAndNode[0],
        uuid.clockSeqAndNode[1],
        uuid.clockSeqAndNode[2],
        uuid.clockSeqAndNode[3],
        uuid.clockSeqAndNode[4],
        uuid.clockSeqAndNode[5],
        uuid.clockSeqAndNode[6],
        uuid.clockSeqAndNode[7],
    );
    let path = format!("/tee/ta/{ta_name}");
    let args = vec![path.clone()];
    let mut uspace = memspace::MmSpace::new_user_empty()?;
    let (entry, stack_top, saved_aux) = load_user_elf_image(&mut uspace, &image, &args, &[])?;
    drop(image);

    let page_table_root = uspace.page_table_hw_root();
    let uctx = UserContext::new(entry.into(), stack_top, 0);
    let pid_handle = kidentity::allocate_root_pid_handle()?;
    let process = Process::new_init_with_task_number(pid_handle.clone());
    let thread = build_process_thread(
        process.clone(),
        pid_handle.clone(),
        path.clone(),
        Arc::new(args),
        Arc::new(Mutex::new(uspace)),
        copy_init_fs_struct(),
        Default::default(),
        initial_cred(),
    )?;
    process.with_tee_ta_ctx_mut(|ctx| ctx.init_ta_ctx(&path, &head_bytes))?;
    process.set_saved_aux(saved_aux);

    let entry = prepare_user_entry(uctx, 0, dispatch_syscall)?;
    let mut task = new_user_task(&ta_name, entry, pid_handle, thread);
    task.ctx_mut().set_page_table_root(page_table_root);
    publish_user_task(task)?.commit(|_| Ok(()))?;
    Ok(())
}
