// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/proc/interrupts` over the KIRQ owned Line snapshot.
//!
//! The iterator captures one online-CPU column set per traversal. A Line that
//! disappears between enumeration and snapshot is skipped rather than turned
//! into EOF. Other observation failures remain errors.

use alloc::sync::Arc;
use core::fmt::{self, Write};
#[cfg(unittest)]
use core::sync::atomic::{AtomicU8, Ordering};

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::{LogicalCpuId, for_each_online_logical_cpu};
use kerrno::LinuxError;
use kirq::{
    IrqController, IrqStatsClass, IrqStatsError, IrqStatsSnapshot, IrqTrigger, next_irq_line,
};
use kvfs::{
    NodePermission, SeekEndPolicy, SeqFileInode, SeqFileOptions, SeqIterator, SeqTextSink,
    SimpleFs, VfsError, VfsResult,
};

const IRQ_FIELD_WIDTH: usize = 3;
const COUNT_FIELD_WIDTH: usize = 10;

/// One row of the file. The header is emitted even when no countable Line is
/// currently present, matching the Linux file contract.
pub(crate) enum InterruptRecord {
    Header,
    Line(IrqStatsSnapshot),
}

/// Iterator over the current ordinary IRQ Lines.
pub(crate) struct InterruptsIter {
    cpus: Option<KBox<[LogicalCpuId]>>,
    online_cpus: usize,
    next_cursor: Option<usize>,
    #[cfg(unittest)]
    retire_next: Option<usize>,
}

impl InterruptsIter {
    pub(crate) fn new() -> Self {
        Self {
            cpus: None,
            online_cpus: 0,
            next_cursor: Some(0),
            #[cfg(unittest)]
            retire_next: None,
        }
    }

    #[cfg(unittest)]
    pub(crate) fn new_with_retire_for_tests(virq: usize) -> Self {
        let mut iter = Self::new();
        iter.retire_next = Some(virq);
        iter
    }

    fn capture_cpus(&mut self) -> VfsResult<()> {
        // Allocate the configured maximum rather than racing a separate online
        // count against the traversal. Only the initialized prefix is exposed.
        let mut cpus = KBox::new_slice_with_flags(
            LogicalCpuId::new(0),
            kbuild_config::NR_CPUS,
            AllocFlags::NOWAIT,
        )
        .map_err(|_| VfsError::NoMemory)?;
        let mut count = 0;
        for_each_online_logical_cpu(|index, cpu, _| {
            if index < cpus.len() {
                cpus[index] = cpu;
                count = index + 1;
            }
        });
        if count == 0 {
            return Err(VfsError::from(LinuxError::ENODATA));
        }
        self.cpus = Some(cpus);
        self.online_cpus = count;
        self.next_cursor = Some(0);
        Ok(())
    }

    fn cpus(&self) -> &[LogicalCpuId] {
        match self.cpus.as_ref() {
            Some(cpus) => &cpus[..self.online_cpus],
            None => &[],
        }
    }

    fn prepare(&mut self) -> VfsResult<Option<InterruptRecord>> {
        self.capture_cpus()?;
        Ok(Some(InterruptRecord::Header))
    }

    fn next_line(&mut self) -> VfsResult<Option<InterruptRecord>> {
        loop {
            let Some(cursor) = self.next_cursor else {
                return Ok(None);
            };
            let handle = match next_irq_line(cursor) {
                Ok(Some(handle)) => handle,
                Ok(None) => {
                    self.next_cursor = None;
                    return Ok(None);
                }
                Err(_) => return Err(VfsError::Io),
            };
            let virq = handle.virq();
            self.next_cursor = virq.checked_add(1);
            #[cfg(unittest)]
            if self.retire_next == Some(virq) {
                self.retire_next = None;
                handle.retire_for_tests();
            }
            #[cfg(unittest)]
            if let Some(error) = take_forced_stats_error_for_tests() {
                return Err(map_stats_error(error));
            }
            let snapshot = match handle.stats_snapshot() {
                Ok(snapshot) => snapshot,
                // A retirement/replacement between enumeration and snapshot is
                // a weak row disappearance, not the end of the file.
                Err(IrqStatsError::UnknownIrq) => continue,
                Err(error) => return Err(map_stats_error(error)),
            };
            if snapshot.class() != IrqStatsClass::Countable {
                continue;
            }
            if !snapshot.has_actions() && snapshot.counts().iter().all(|count| *count == 0) {
                continue;
            }
            return Ok(Some(InterruptRecord::Line(snapshot)));
        }
    }
}

impl SeqIterator for InterruptsIter {
    type Item = InterruptRecord;

    fn rewind(&mut self) {
        self.cpus = None;
        self.online_cpus = 0;
        self.next_cursor = Some(0);
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.prepare().ok().flatten()
    }

    fn next(&mut self) -> Option<Self::Item> {
        self.next_line().ok().flatten()
    }

    fn try_start(&mut self) -> VfsResult<Option<Self::Item>> {
        self.prepare()
    }

    fn try_next(&mut self) -> VfsResult<Option<Self::Item>> {
        if self.cpus.is_none() {
            return self.prepare();
        }
        self.next_line()
    }

    fn show(&self, item: &Self::Item, out: &mut alloc::string::String) -> fmt::Result {
        show_record(self, item, out)
    }

    fn try_show_into(&self, item: &Self::Item, out: &mut dyn SeqTextSink) -> VfsResult<()> {
        let mut writer = SinkWriter { out, error: None };
        let result = show_record(self, item, &mut writer);
        if let Some(error) = writer.error {
            return Err(error);
        }
        result.map_err(|_| VfsError::Io)
    }
}

#[cfg(unittest)]
static FORCED_STATS_ERROR: AtomicU8 = AtomicU8::new(0);

#[cfg(unittest)]
pub(crate) fn force_next_stats_error_for_tests(error: Option<IrqStatsError>) {
    let code = match error {
        None => 0,
        Some(IrqStatsError::NoMemory) => 1,
        Some(IrqStatsError::CapacityChanged) => 2,
        Some(IrqStatsError::Overflow) => 3,
        Some(IrqStatsError::InvalidContext) => 4,
        Some(IrqStatsError::UnknownIrq) => 5,
    };
    FORCED_STATS_ERROR.store(code, Ordering::Relaxed);
}

#[cfg(unittest)]
fn take_forced_stats_error_for_tests() -> Option<IrqStatsError> {
    match FORCED_STATS_ERROR.swap(0, Ordering::Relaxed) {
        1 => Some(IrqStatsError::NoMemory),
        2 => Some(IrqStatsError::CapacityChanged),
        3 => Some(IrqStatsError::Overflow),
        4 => Some(IrqStatsError::InvalidContext),
        5 => Some(IrqStatsError::UnknownIrq),
        _ => None,
    }
}

fn map_stats_error(error: IrqStatsError) -> VfsError {
    match error {
        IrqStatsError::UnknownIrq => VfsError::NotFound,
        IrqStatsError::NoMemory => VfsError::NoMemory,
        IrqStatsError::CapacityChanged => VfsError::WouldBlock,
        IrqStatsError::Overflow => VfsError::from(LinuxError::EOVERFLOW),
        IrqStatsError::InvalidContext => VfsError::Io,
    }
}

fn show_record(iter: &InterruptsIter, item: &InterruptRecord, out: &mut dyn Write) -> fmt::Result {
    match item {
        InterruptRecord::Header => {
            write!(out, "{:width$}", "", width = IRQ_FIELD_WIDTH + 8)?;
            for cpu in iter.cpus() {
                write!(out, "CPU{:<8}", cpu.as_usize())?;
            }
            writeln!(out)
        }
        InterruptRecord::Line(snapshot) => {
            write!(out, "{:>width$}:", snapshot.virq(), width = IRQ_FIELD_WIDTH)?;
            for cpu in iter.cpus() {
                let count = snapshot
                    .counts()
                    .get(cpu.as_usize())
                    .copied()
                    .unwrap_or_default();
                write!(out, " {:>width$}", count, width = COUNT_FIELD_WIDTH)?;
            }
            write!(
                out,
                "  {} {}  {}",
                controller_name(snapshot.controller()),
                snapshot.hwirq(),
                trigger_name(snapshot.trigger())
            )?;
            for (index, name) in snapshot.action_names().iter().enumerate() {
                if index == 0 {
                    write!(out, "  {name}")?;
                } else {
                    write!(out, ", {name}")?;
                }
            }
            writeln!(out)
        }
    }
}

fn controller_name(controller: IrqController) -> &'static str {
    match controller {
        IrqController::Gic => "GIC",
        IrqController::Plic => "PLIC",
        IrqController::IoApic => "IO-APIC",
        IrqController::Msi => "MSI",
        IrqController::LoongArchExtioi => "ExtIOI",
        IrqController::Unknown => "unknown",
    }
}

fn trigger_name(trigger: IrqTrigger) -> &'static str {
    match trigger {
        IrqTrigger::EdgeRising | IrqTrigger::EdgeFalling => "Edge",
        IrqTrigger::LevelHigh | IrqTrigger::LevelLow => "Level",
        IrqTrigger::Unknown(_) => "Unknown",
    }
}

struct SinkWriter<'a> {
    out: &'a mut dyn SeqTextSink,
    error: Option<VfsError>,
}

impl Write for SinkWriter<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.error.is_some() {
            return Err(fmt::Error);
        }
        if let Err(error) = self.out.try_reserve(value.len()) {
            self.error = Some(error);
            return Err(fmt::Error);
        }
        self.out.write_str(value).map_err(|_| {
            self.error = Some(VfsError::Io);
            fmt::Error
        })
    }
}

pub(crate) fn new(fs: Arc<SimpleFs>) -> Arc<SeqFileInode<InterruptsIter>> {
    let options = SeqFileOptions {
        permission: NodePermission::from_bits_truncate(0o444),
        lazy_open: true,
        max_record_bytes: isize::MAX as usize,
        reported_size: 0,
        seek_end: SeekEndPolicy::Reject,
        validate_open: None,
        validate_read: None,
        command: None,
    };
    SeqFileInode::new_regular_with_options(fs, || Ok(InterruptsIter::new()), options)
}
