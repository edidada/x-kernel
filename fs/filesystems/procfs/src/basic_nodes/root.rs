// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;

#[cfg(not(target_arch = "aarch64"))]
use kbuild_config::ARCH;
use kcpu_id_map::for_each_present_logical_cpu;
use kvfs::{DirMapping, SimpleDir, SimpleFile, SimpleFs};

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
                #[cfg(target_arch = "aarch64")]
                writeln!(
                    info,
                    "processor\t: {}\nBogoMIPS\t: 100.00\nFeatures\t: fp asimd evtstrm aes pmull \
                     sha1 sha2 crc32 atomics cpuid\nCPU implementer\t: 0x41\nCPU architecture\t: \
                     8\nCPU variant\t: 0x0\nCPU part\t: 0xd0b\nCPU revision\t: 0",
                    cpu_id.as_usize(),
                )
                .unwrap();
                #[cfg(not(target_arch = "aarch64"))]
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
            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });
}
