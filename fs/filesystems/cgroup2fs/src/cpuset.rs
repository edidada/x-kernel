// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Control files and scheduler bridge for the `cpuset` controller.
//!
//! The controller itself owns mask validation and ancestor intersection in
//! `kcgroup`. This adapter only parses the Linux cpulist format and applies the
//! resulting effective masks to the tasks whose cgroup membership changed.
//! The cgroup mask is a bound layered on top of the mask userspace requested
//! with `sched_setaffinity`; applying it never overwrites that request. A task
//! whose request does not intersect the new bound fails the operation with
//! `ENOSPC` and keeps its previous masks.

use alloc::{format, string::String, sync::Arc, vec::Vec};

use kcgroup::{Cgroup, CpuMask, NodeMask};
use kprocess::Pid;
use kvfs::{CommandFile, SimpleFile, SimpleFileOperation, SimpleFs, VfsError, VfsResult};

use crate::{command, control};

pub(crate) fn files(
    fs: Arc<SimpleFs>,
    cgroup: Arc<Cgroup>,
) -> Vec<(&'static str, Arc<SimpleFile>)> {
    alloc::vec![
        (
            "cpuset.cpus",
            cpus_file(fs.clone(), cgroup.clone(), CpusetField::Cpus),
        ),
        (
            "cpuset.cpus.effective",
            effective_file(fs.clone(), cgroup.clone(), CpusetField::Cpus),
        ),
        (
            "cpuset.mems",
            cpus_file(fs.clone(), cgroup.clone(), CpusetField::Mems),
        ),
        (
            "cpuset.mems.effective",
            effective_file(fs, cgroup, CpusetField::Mems),
        ),
    ]
}

#[derive(Clone, Copy)]
enum CpusetField {
    Cpus,
    Mems,
}

impl CpusetField {
    fn read(self, cgroup: &Cgroup, is_effective: bool) -> VfsResult<String> {
        let mask = match (self, is_effective) {
            (Self::Cpus, false) => cgroup.cpuset_cpus()?,
            (Self::Cpus, true) => cgroup.cpuset_effective_cpus()?,
            (Self::Mems, false) => cgroup.cpuset_mems()?,
            (Self::Mems, true) => cgroup.cpuset_effective_mems()?,
        };
        Ok(format!("{}\n", mask.format_list()))
    }

    fn write(self, cgroup: &Arc<Cgroup>, text: &str) -> VfsResult<()> {
        match self {
            Self::Cpus => {
                let mask = CpuMask::parse_list(text)?;
                cgroup.set_cpuset_cpus(mask)?;
                apply_cpuset_affinity_to_subtree(cgroup)?;
            }
            Self::Mems => {
                let mask = NodeMask::parse_list(text)?;
                cgroup.set_cpuset_mems(mask)?;
                apply_cpuset_affinity_to_subtree(cgroup)?;
            }
        }
        Ok(())
    }
}

fn cpus_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>, field: CpusetField) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(
        fs,
        control::writable_mode(),
        CommandFile::new(move |operation| match operation {
            SimpleFileOperation::Read => {
                let _operation = cgroup.begin_operation()?;
                Ok(Some(field.read(&cgroup, false)?))
            }
            SimpleFileOperation::Write { data, .. } => {
                let _operation = cgroup.begin_operation()?;
                let value = command::parse_command(data)?;
                field.write(&cgroup, value)?;
                Ok(None)
            }
        }),
    )
}

fn effective_file(fs: Arc<SimpleFs>, cgroup: Arc<Cgroup>, field: CpusetField) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, control::readonly_mode(), move || {
        let _operation = cgroup.begin_operation()?;
        field.read(&cgroup, true)
    })
}

/// Applies `cgroup`'s effective CPU and memory-node masks to every process
/// directly in it.
pub(crate) fn apply_cpuset_affinity(cgroup: &Arc<Cgroup>) -> VfsResult<()> {
    let Ok(cpus) = cgroup.cpuset_effective_cpus() else {
        return Ok(());
    };
    let Ok(mems) = cgroup.cpuset_effective_mems() else {
        return Ok(());
    };
    apply_to_members(cgroup, cpus, mems)
}

/// Applies `cgroup`'s effective masks to one process after migration.
pub(crate) fn apply_cpuset_affinity_to_process(cgroup: &Arc<Cgroup>, pid: Pid) -> VfsResult<()> {
    // A target cgroup may have no active cpuset controller. In that case the
    // migration must not fail after membership has already been committed;
    // the task inherits the ancestor's existing affinity until cpuset is
    // enabled later.
    let Ok(cpus) = cgroup.cpuset_effective_cpus() else {
        return Ok(());
    };
    let Ok(mems) = cgroup.cpuset_effective_mems() else {
        return Ok(());
    };
    apply_to_process(pid, kernel_cpu_mask(cpus), kernel_node_mask(mems))
}

fn apply_cpuset_affinity_to_subtree(cgroup: &Arc<Cgroup>) -> VfsResult<()> {
    apply_cpuset_affinity(cgroup)?;
    for descendant in cgroup.descendants() {
        apply_cpuset_affinity(&descendant)?;
    }
    Ok(())
}

fn apply_to_members(cgroup: &Arc<Cgroup>, cpus: CpuMask, mems: NodeMask) -> VfsResult<()> {
    let cpus = kernel_cpu_mask(cpus);
    let mems = kernel_node_mask(mems);
    for pid in kprocess::cgroup_member_process_ids(cgroup) {
        apply_to_process(pid, cpus, mems)?;
    }
    Ok(())
}

fn apply_to_process(pid: Pid, cpus: ktask::KCpuMask, mems: ktask::KCpuMask) -> VfsResult<()> {
    let Ok(process) = kprocess::scheduler::target_process(pid) else {
        return Ok(());
    };
    for task in kprocess::scheduler::process_tasks(&process) {
        if !ktask::set_task_cgroup_cpus(&task, cpus) {
            return Err(VfsError::StorageFull);
        }
        ktask::set_task_mems(&task, mems);
    }
    Ok(())
}

fn kernel_cpu_mask(mask: CpuMask) -> ktask::KCpuMask {
    copy_mask(mask.iter())
}

fn kernel_node_mask(mask: NodeMask) -> ktask::KCpuMask {
    copy_mask(mask.iter())
}

fn copy_mask(indices: impl Iterator<Item = usize>) -> ktask::KCpuMask {
    let mut result = ktask::KCpuMask::new();
    let capacity = ktask::KCpuMask::full()
        .last_index()
        .map_or(0, |last_index| last_index + 1);
    for index in indices {
        if index < capacity {
            result.set(index, true);
        }
    }
    result
}
