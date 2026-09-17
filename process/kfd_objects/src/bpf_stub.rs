// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Placeholder object for `bpf(2)` program fds handed out by the stubbed
//! device-controller frontend.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::KResult;
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile};

/// Opaque marker identifying a stubbed `bpf(2)` program fd.
///
/// The kernel runs no eBPF interpreter, so a loaded program is modelled as
/// this empty private-data object: the fd is a real anonymous-inode file, so
/// `close(2)`/`dup(2)` behave normally for user space.
pub struct BpfProgramPlaceholder;

struct BpfProgramPlaceholderFops;

impl FileOperations for BpfProgramPlaceholderFops {}

impl BpfProgramPlaceholder {
    /// Mints the anonymous-inode file returned by the stubbed
    /// `BPF_PROG_LOAD` / `BPF_LINK_CREATE`.
    pub fn new_file(cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        AnonInodeFs::global().get_file(
            "[bpf]",
            Arc::new(BpfProgramPlaceholderFops),
            Arc::new(BpfProgramPlaceholder),
            FMode::READ | FMode::WRITE,
            OpenFlags::empty(),
            cred,
        )
    }
}
