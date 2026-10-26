// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;

use kbuild_config::ARCH;
use kcpu_id_map::for_each_present_logical_cpu;
use kvfs::{
    CommandFile, DirMapping, NodePermission, SimpleDir, SimpleFile, SimpleFileOperation, SimpleFs,
    VfsError,
};

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: linear registration table: each statement registers one \
              independent procfs root entry"
)]
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "cmdline",
        SimpleFile::new_regular(fs.clone(), || {
            Ok(match khal::cmdline() {
                Some(cmdline) if !cmdline.is_empty() => format!("{cmdline}\n"),
                _ => String::from("\n"),
            })
        }),
    );
    root.add(
        "instret",
        SimpleFile::new_regular(fs.clone(), || {
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            {
                Ok(format!("{}\n", riscv::register::instret::read64()))
            }
            #[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
            {
                Ok(String::from("0\n"))
            }
        }),
    );
    root.add(
        "cpuinfo",
        SimpleFile::new_regular(fs.clone(), || {
            let mut info = String::new();
            for_each_present_logical_cpu(|present_index, cpu_id, cpu_count| {
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: String writes are infallible \
                              and the formatted integers and strings cannot return fmt::Error"
                )]
                writeln!(
                    info,
                    "processor\t: {}\ncpu cores\t: {}\narchitecture\t: {}",
                    cpu_id.as_usize(),
                    cpu_count,
                    ARCH,
                )
                .unwrap();
                if present_index + 1 < cpu_count {
                    info.push('\n');
                }
            });
            Ok(info)
        }),
    );
    root.add(
        "filesystems",
        SimpleFile::new_regular(fs.clone(), || {
            let mut info = String::new();
            for file_system_type in kvfs::registered_filesystems() {
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: String writes are infallible \
                              and the formatted integers and strings cannot return fmt::Error"
                )]
                writeln!(
                    info,
                    "{}\t{}",
                    if file_system_type.requires_device() {
                        ""
                    } else {
                        "nodev"
                    },
                    file_system_type.name(),
                )
                .unwrap();
            }
            Ok(info)
        }),
    );
    root.add("sys", {
        let mut sys = DirMapping::new();

        sys.add("kernel", {
            let mut kernel = DirMapping::new();
            kernel.add(
                "pid_max",
                SimpleFile::new_regular(fs.clone(), || Ok("32768\n")),
            );
            kernel.add("pty", {
                let mut pty = DirMapping::new();
                let permission = NodePermission::from_bits_truncate(0o444);
                pty.add(
                    "max",
                    SimpleFile::new_regular_with_permission(fs.clone(), permission, || {
                        Ok(format!("{}\n", ktty::tty::pty_limit()))
                    }),
                );
                pty.add(
                    "nr",
                    SimpleFile::new_regular_with_permission(fs.clone(), permission, || {
                        Ok(format!("{}\n", ktty::tty::pty_allocated_count()))
                    }),
                );
                SimpleDir::new_maker(fs.clone(), Arc::new(pty))
            });
            // Runtime log-filter control: reading returns the canonical text
            // of the active spec (`warn ksyscall=debug`); a privileged write
            // installs a whole spec — an optional bare level for the default
            // plus `target=level` overrides — replacing the default level and
            // the entire override set, or, when the spec carries no bare level,
            // only the override set. Either way it takes effect immediately and
            // needs no rebuild.
            //
            // NOTE: although this node reuses the Linux `/proc/sys/kernel/
            // printk` path, the value domain is X-Kernel-private and NOT
            // Linux `console_loglevel`-compatible: levels here are
            // `0..=5`/`off..trace` with higher = more verbose, while Linux's
            // read side reports four console numbers instead of a filter spec.
            kernel.add(
                "printk",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::OWNER_READ
                        | NodePermission::OWNER_WRITE
                        | NodePermission::GROUP_READ
                        | NodePermission::OTHER_READ,
                    CommandFile::new(|operation| match operation {
                        SimpleFileOperation::Read => {
                            let mut buf = [0u8; klogger::MAX_FILTER_SPEC];
                            let len =
                                klogger::filter_spec(&mut buf).ok_or(VfsError::InvalidInput)?;
                            let spec = core::str::from_utf8(&buf[..len])
                                .map_err(|_| VfsError::InvalidInput)?;
                            Ok(Some(format!("{spec}\n")))
                        }
                        SimpleFileOperation::Write { file, data } => {
                            if !file.cred().is_privileged() {
                                return Err(VfsError::OperationNotPermitted);
                            }
                            let text =
                                core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                            klogger::set_filter_spec(text).map_err(|_| VfsError::InvalidInput)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });
}

unittest::path_mod_test!(tests, "../../ut-tests/basic_nodes_root.rs");
