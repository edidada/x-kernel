// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc};

use kvfs::{
    DirMapping, FileSystemType, Filename, Mount, NodePermission, OpenFlags, SimpleDir, SimpleFs,
    VfsError,
};
use unittest::{assert, assert_eq, def_test};

use super::add_root_entries;

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("procfs-printk-test");

#[def_test(serial)]
fn pty_nodes_report_limits_and_reject_root_writes() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    let root = Mount::new_root(&super_block).root_path();
    for (path, expected) in [
        ("/sys/kernel/pty/max", ktty::tty::pty_limit()),
        ("/sys/kernel/pty/nr", ktty::tty::pty_allocated_count()),
    ] {
        // Root bypasses mode checks, so writes exercise the read-only backend.
        let file = Filename::new(path)
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::READ_WRITE.bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();
        assert!(file.cred().is_privileged());
        assert_eq!(
            file.path().getattr().unwrap().mode.permission().bits(),
            0o444
        );
        let expected = format!("{expected}\n");
        let mut buf = [0u8; 32];
        let mut pos = 0;
        let count = file.read_from(&mut buf, &mut pos).unwrap();
        assert_eq!(&buf[..count], expected.as_bytes());
        assert_eq!(file.write(b"0\n"), Err(VfsError::BadFileDescriptor));
    }
}

/// `serial`: the printk node owns the process-global log filter, so it must
/// not interleave with another test that reads or writes the same filter.
#[def_test(serial)]
fn printk_node_reads_and_writes_runtime_log_level() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    let root = Mount::new_root(&super_block).root_path();
    let printk = Filename::new("/sys/kernel/printk")
        .open_with_flags_at(
            &root,
            &root,
            OpenFlags::READ_WRITE.bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    // Restore whatever filter the boot (or a previous test) installed.
    let mut previous_buf = [0u8; klogger::MAX_FILTER_SPEC];
    let previous_len = klogger::filter_spec(&mut previous_buf).unwrap_or(0);

    // A level number (with trailing newline, as `echo` writes it) takes
    // effect immediately...
    assert_eq!(printk.write(b"4\n"), Ok(2));
    assert_eq!(klogger::current_log_level(), log::LevelFilter::Debug);
    // ...and so does a level name.
    assert_eq!(printk.write(b"warn"), Ok(4));
    assert_eq!(klogger::current_log_level(), log::LevelFilter::Warn);

    // Reading reports the canonical spec of the active filter.
    // (`read_from` with an explicit position sidesteps the file offset
    // advanced by the writes above.)
    let mut buf = [0u8; 64];
    let mut pos = 0;
    assert_eq!(printk.read_from(&mut buf, &mut pos), Ok(5));
    assert_eq!(&buf[..5], b"warn\n");

    // A spec carries per-target overrides, which is what makes targeted
    // bring-up logging possible without a rebuild.
    assert_eq!(printk.write(b"warn drv_uart=trace"), Ok(19));
    assert_eq!(klogger::current_log_level(), log::LevelFilter::Warn);
    let mut pos = 0;
    assert_eq!(printk.read_from(&mut buf, &mut pos), Ok(20));
    assert_eq!(&buf[..20], b"warn drv_uart=trace\n");

    // Invalid or out-of-range levels are rejected and leave the filter
    // unchanged (a bad token must not half-apply a spec).
    assert!(printk.write(b"banana").is_err());
    assert!(printk.write(b"6").is_err());
    assert!(printk.write(b"warn bogus=nonsense").is_err());
    assert_eq!(klogger::current_log_level(), log::LevelFilter::Warn);
    let mut pos = 0;
    assert_eq!(printk.read_from(&mut buf, &mut pos), Ok(20));
    assert_eq!(&buf[..20], b"warn drv_uart=trace\n");

    if let Ok(text) = core::str::from_utf8(&previous_buf[..previous_len]) {
        klogger::set_log_level(text);
    } else {
        klogger::set_log_level_filter(log::LevelFilter::Warn);
    }
}

/// Linux installs `proc_reg_file_ops` (whose `proc_reg_poll` falls back to
/// `DEFAULT_POLLMASK`) on the regular-file shape of a `/proc` entry, so those
/// entries answer `file_can_poll` and `epoll_ctl` accepts them. `/cmdline` is
/// the generated form and `/meminfo` the iterator form; both are regular files
/// and must be pollable.
///
/// The shape is what matters: a proc symlink inode is left with `i_fop = NULL`
/// (`fs/proc/inode.c`) and an `O_PATH` open installs `EmptyFileOperations`, so
/// neither answers `file_can_poll` — this test must not be read as licence to
/// give a poll handler to `/proc/<pid>/ns/*`, `exe` or `fd/N`.
#[def_test(serial)]
fn proc_nodes_are_pollable_like_linux() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        crate::mem_nodes::root::add_root_entries(&mut root, fs.clone());
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    let root = Mount::new_root(&super_block).root_path();

    for path in ["/cmdline", "/meminfo"] {
        let file = Filename::new(path)
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::empty().bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();
        assert!(file.can_poll(), "{path} must be pollable");
    }
}
