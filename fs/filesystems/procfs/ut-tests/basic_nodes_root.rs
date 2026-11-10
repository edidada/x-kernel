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
