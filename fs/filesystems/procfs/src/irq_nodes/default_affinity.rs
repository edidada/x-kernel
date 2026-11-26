// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/proc/irq/default_smp_affinity`: bitmap view over KIRQ's global default.

use alloc::{string::String, sync::Arc};
use core::fmt;

use kcpu_id_map::{
    KCpuMask,
    text::{CpuTextDomain, bitmap_capacity, parse_bitmap, write_bitmap},
};
use kerrno::LinuxError;
use kirq::{DefaultSmpAffinityError, default_smp_affinity, set_default_smp_affinity};
use kvfs::{
    EmptyCommandPolicy, NodeFlags, NodePermission, SeekEndPolicy, SeqCommandOptions, SeqFileInode,
    SeqFileOptions, SeqIterator, SimpleFs, VfsError, VfsInode, VfsResult,
};

use super::file::MAX_COMMAND_BYTES;

pub(crate) const DEFAULT_SMP_AFFINITY_FILE: &str = "default_smp_affinity";
const DEFAULT_MODE: NodePermission = NodePermission::from_bits_truncate(0o644);

struct DefaultAffinityIter {
    domain: CpuTextDomain,
    emitted: bool,
}

impl SeqIterator for DefaultAffinityIter {
    type Item = KCpuMask;

    fn rewind(&mut self) {
        self.emitted = false;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.try_start().ok().flatten()
    }

    fn next(&mut self) -> Option<Self::Item> {
        None
    }

    fn try_start(&mut self) -> VfsResult<Option<Self::Item>> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        default_smp_affinity()
            .map(Some)
            .ok_or_else(|| VfsError::from(LinuxError::ENODATA))
    }

    fn try_next(&mut self) -> VfsResult<Option<Self::Item>> {
        Ok(None)
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        self.try_show(item, buf).map_err(|_| fmt::Error)
    }

    fn try_show(&self, item: &Self::Item, buf: &mut String) -> VfsResult<()> {
        let capacity_bytes =
            bitmap_capacity(&self.domain).map_err(crate::cpu_text_error::map_cpu_text)?;
        buf.try_reserve(capacity_bytes.checked_add(1).ok_or(VfsError::NoMemory)?)
            .map_err(|_| VfsError::NoMemory)?;
        write_bitmap(*item, &self.domain, buf).map_err(crate::cpu_text_error::map_cpu_text)?;
        buf.push('\n');
        Ok(())
    }
}

/// Creates the stable root file inode. The owning namespace retains it.
pub(crate) fn new_inode(fs: Arc<SimpleFs>) -> VfsResult<Arc<VfsInode>> {
    let domain = CpuTextDomain::runtime().map_err(crate::cpu_text_error::map_cpu_text)?;
    let max_record_bytes = bitmap_capacity(&domain)
        .map_err(crate::cpu_text_error::map_cpu_text)?
        .checked_add(1)
        .ok_or(VfsError::NoMemory)?;
    let write_domain = domain.clone();
    let options = SeqFileOptions {
        permission: DEFAULT_MODE,
        lazy_open: true,
        max_record_bytes,
        reported_size: 0,
        seek_end: SeekEndPolicy::Reject,
        validate_open: None,
        validate_read: None,
        command: Some(SeqCommandOptions {
            max_command_bytes: MAX_COMMAND_BYTES,
            validate_write: None,
            write: Arc::new(move |_file, data| {
                let mask = parse_bitmap(data, &write_domain)
                    .map_err(|_| VfsError::from(LinuxError::EINVAL))?;
                set_default_smp_affinity(&mask).map_err(map_default_affinity_write)
            }),
            empty: EmptyCommandPolicy::Callback,
        }),
    };
    let iter_domain = domain;
    let seq = SeqFileInode::new_regular_with_options(
        fs,
        move || {
            Ok(DefaultAffinityIter {
                domain: iter_domain.clone(),
                emitted: false,
            })
        },
        options,
    );
    let init = seq.inode_init();
    Ok(VfsInode::new_file_with_flags(
        seq,
        NodeFlags::NON_CACHEABLE,
        init,
    ))
}

pub(crate) fn map_default_affinity_write(error: DefaultSmpAffinityError) -> VfsError {
    match error {
        DefaultSmpAffinityError::NotReady => VfsError::from(LinuxError::ENODEV),
        DefaultSmpAffinityError::Empty | DefaultSmpAffinityError::NotPresent { .. } => {
            VfsError::from(LinuxError::EINVAL)
        }
        DefaultSmpAffinityError::InvalidContext { .. } => VfsError::from(LinuxError::EPERM),
    }
}
