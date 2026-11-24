// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/proc/interrupts` presentation and `SeqFile` behaviour.

use alloc::{format, string::String, sync::Arc, vec::Vec};

use iov_iter::IovSink;
use kcpu_id_map::{LogicalCpuId, for_each_online_logical_cpu};
use kirq::{IrqController, IrqDesc, IrqTrigger, tests_irq::SyntheticLine};
use kvfs::{
    Filename, Mount, NodePermission, NodeType, OpenFlags, SeqFile, SuperBlockFlags, VfsError,
};
use unittest::{TestResult, assert, assert_eq, def_test};

static FLOW: kirq::FasteoiChip = kirq::FasteoiChip {
    resend: kirq::FasteoiResend::Software,
    mask: |_| {},
    unmask: |_| {},
};

fn online_cpus() -> Vec<LogicalCpuId> {
    let mut cpus = Vec::new();
    for_each_online_logical_cpu(|_, cpu, _| cpus.push(cpu));
    cpus
}

fn interrupts_file() -> alloc::sync::Arc<kvfs::VfsFile> {
    let fs = crate::new_procfs(SuperBlockFlags::empty()).unwrap();
    let root = Mount::new_root(&fs).root_path();
    Filename::new("/interrupts")
        .open_with_flags_at(
            &root,
            &root,
            OpenFlags::empty().bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap()
}

fn read_all(file: &kvfs::VfsFile) -> String {
    let mut text = String::new();
    let mut buffer = [0u8; 37];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        text.push_str(core::str::from_utf8(&buffer[..count]).unwrap());
    }
    text
}

fn mapped(virq: usize, trigger: IrqTrigger) -> SyntheticLine {
    let desc = IrqDesc::new(0x2a, trigger)
        .with_virq(virq)
        .with_controller(IrqController::Gic);
    let line = SyntheticLine::mapped(desc);
    line.set_flow_for_tests(kirq::IrqFlow::Fasteoi(&FLOW));
    line
}

struct TwoSliceSink<'a> {
    first: &'a mut [u8],
    second: &'a mut [u8],
    first_used: usize,
    second_used: usize,
}

impl IovSink for TwoSliceSink<'_> {
    fn count(&self) -> usize {
        (self.first.len() - self.first_used) + (self.second.len() - self.second_used)
    }

    fn copy_to_iter(&mut self, source: &[u8]) -> kerrno::KResult<usize> {
        let mut copied = 0;
        while copied < source.len() && self.first_used < self.first.len() {
            self.first[self.first_used] = source[copied];
            self.first_used += 1;
            copied += 1;
        }
        while copied < source.len() && self.second_used < self.second.len() {
            self.second[self.second_used] = source[copied];
            self.second_used += 1;
            copied += 1;
        }
        Ok(copied)
    }

    fn copy_to_iter_nofault(&mut self, source: &[u8]) -> kerrno::KResult<usize> {
        self.copy_to_iter(source)
    }
}

fn assert_header(text: &str, cpus: &[LogicalCpuId]) {
    let header = text.lines().next().expect("header");
    core::assert_eq!(
        header,
        format!(
            "{:width$}{}",
            "",
            cpus.iter()
                .map(|cpu| format!("CPU{:<8}", cpu.as_usize()))
                .collect::<String>(),
            width = 12
        )
    );
}

#[def_test(serial)]
fn header_always_present_and_named_line_is_formatted() -> unittest::TestResult {
    let cpus = online_cpus();
    assert!(!cpus.is_empty());
    let line = mapped(0x7401, IrqTrigger::LevelHigh);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert!(
        line.install_shared_named_action_for_tests(handler, "serial")
            .is_some()
    );
    line.set_stats_count_for_tests(cpus[0].as_usize(), 7);

    let text = read_all(&interrupts_file());
    assert_header(&text, &cpus);
    let row = text
        .lines()
        .find(|row| row.starts_with(&format!("{:>4}:", line.virq())))
        .expect("IRQ row");
    let prefix = format!("{:>4}:", line.virq());
    assert!(row.starts_with(&prefix));
    let count_start = prefix.len();
    for index in 0..cpus.len() {
        let expected = if index == 0 { 7 } else { 0 };
        assert_eq!(
            &row[count_start + index * 11..count_start + (index + 1) * 11],
            format!(" {:>10}", expected)
        );
    }
    let fields_start = count_start + cpus.len() * 11;
    assert_eq!(
        &row[fields_start..],
        format!("  {:<8} {:>4} {:<8}  serial", "GIC", 42, "Level")
    );
    assert_eq!(text.lines().filter(|row| row.trim().is_empty()).count(), 0);
    TestResult::Ok
}

#[def_test(serial)]
fn no_action_zero_line_is_hidden_but_historical_counts_are_visible() -> unittest::TestResult {
    let zero = mapped(0x7402, IrqTrigger::LevelHigh);
    let historical = mapped(0x7403, IrqTrigger::LevelHigh);
    historical.set_stats_count_for_tests(online_cpus()[0].as_usize(), 9);
    let text = read_all(&interrupts_file());
    assert!(!text.contains(&format!("{}:", zero.virq())));
    let row = text
        .lines()
        .find(|row| row.starts_with(&format!("{:>4}:", historical.virq())))
        .expect("historical row");
    assert!(row.contains(" 9"));
    assert!(!row.contains("serial"));
    TestResult::Ok
}

#[def_test(serial)]
fn nmi_and_no_flow_lines_are_skipped() -> unittest::TestResult {
    let nmi = mapped(0x7404, IrqTrigger::LevelHigh);
    nmi.set_flow_for_tests(kirq::IrqFlow::PerCpu);
    nmi.install_nmi_action_for_tests();
    nmi.set_stats_count_for_tests(online_cpus()[0].as_usize(), 4);

    let no_flow = SyntheticLine::mapped(IrqDesc::from_virq(0x7405));
    no_flow.clear_flow_for_tests();
    no_flow.set_stats_count_for_tests(online_cpus()[0].as_usize(), 5);

    let text = read_all(&interrupts_file());
    assert!(!text.contains(&format!("{}:", nmi.virq())));
    assert!(!text.contains(&format!("{}:", no_flow.virq())));
    TestResult::Ok
}

#[def_test(serial)]
fn unknown_irq_between_enumeration_and_snapshot_is_skipped() -> unittest::TestResult {
    let retired = mapped(0x740c, IrqTrigger::LevelHigh);
    let retired_virq = retired.virq();
    let live = mapped(0x740d, IrqTrigger::LevelHigh);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert!(
        live.install_shared_named_action_for_tests(handler, "after-unknown")
            .is_some()
    );

    let iter =
        crate::irq_nodes::interrupts::InterruptsIter::new_with_retire_for_tests(retired_virq);
    let mut seq = SeqFile::try_with_capacity(iter, 4096, isize::MAX as usize).unwrap();
    let mut text = String::new();
    let mut buffer = [0u8; 37];
    let mut offset = 0;
    loop {
        let count = seq.read(&mut buffer, offset).unwrap();
        if count == 0 {
            break;
        }
        text.push_str(core::str::from_utf8(&buffer[..count]).unwrap());
        offset += count as u64;
    }
    assert!(!text.contains(&format!("{}:", retired_virq)));
    assert!(text.contains(&format!("{}:", live.virq())));
    assert!(text.contains("after-unknown"));
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn retired_row_is_skipped_without_ending_the_file() -> unittest::TestResult {
    let retired = mapped(0x7406, IrqTrigger::LevelHigh);
    retired.retire_for_tests();
    let live = mapped(0x7407, IrqTrigger::LevelHigh);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert!(
        live.install_shared_named_action_for_tests(handler, "live")
            .is_some()
    );

    let text = read_all(&interrupts_file());
    assert!(!text.contains(&format!("{}:", retired.virq())));
    assert!(text.contains(&format!("{}:", live.virq())));
    assert!(text.contains("live"));
    TestResult::Ok
}

#[def_test(serial)]
fn partial_read_pread_rewind_and_read_iter_agree() -> unittest::TestResult {
    let line = mapped(0x7408, IrqTrigger::EdgeRising);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert!(
        line.install_shared_named_action_for_tests(handler, "chars")
            .is_some()
    );

    let file = interrupts_file();
    let full = read_all(&file);
    assert!(full.starts_with("            CPU"));

    assert_eq!(file.llseek(0, 0).unwrap(), 0);
    let mut small = [0u8; 3];
    let first = file.read(&mut small).unwrap();
    assert_eq!(first, 3);
    assert_eq!(&small, &full.as_bytes()[..3]);

    assert_eq!(file.llseek(0, 0).unwrap(), 0);
    let mut read_buffer = [0u8; 32];
    let mut reader = iov_iter::iov_iter_kvec_dest(&mut read_buffer);
    let count = file.read_iter(&mut reader).unwrap();
    assert!(count > 0);

    assert_eq!(file.llseek(0, 0).unwrap(), 0);
    let mut pos = 0;
    let mut pread = [0u8; 12];
    let count = file.read_from(&mut pread, &mut pos).unwrap();
    assert_eq!(&pread[..count], &full.as_bytes()[..count]);

    assert_eq!(file.llseek(0, 0).unwrap(), 0);
    assert_eq!(read_all(&file), full);
    TestResult::Ok
}

#[def_test(serial)]
fn readv_two_segments_matches_contiguous_read() -> unittest::TestResult {
    let line = mapped(0x740a, IrqTrigger::LevelHigh);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert!(
        line.install_shared_named_action_for_tests(handler, "readv")
            .is_some()
    );

    let file = interrupts_file();
    let full = read_all(&file);
    assert_eq!(file.llseek(0, 0).unwrap(), 0);
    let mut first = [0u8; 5];
    let mut second = [0u8; 40];
    let (count, got) = {
        let mut sink = TwoSliceSink {
            first: &mut first,
            second: &mut second,
            first_used: 0,
            second_used: 0,
        };
        let mut iter = iov_iter::iov_iter_dest(&mut sink);
        let count = file.read_iter(&mut iter).unwrap();
        let mut got = Vec::new();
        got.extend_from_slice(&sink.first[..sink.first_used]);
        got.extend_from_slice(&sink.second[..sink.second_used]);
        (count, got)
    };
    assert_eq!(count, got.len());
    assert_eq!(got.as_slice(), &full.as_bytes()[..count]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn observation_errors_are_not_swallowed_as_eof() -> unittest::TestResult {
    let line = mapped(0x740b, IrqTrigger::LevelHigh);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    assert!(
        line.install_shared_named_action_for_tests(handler, "errors")
            .is_some()
    );

    for (error, expected) in [
        (kirq::IrqStatsError::NoMemory, VfsError::NoMemory),
        (kirq::IrqStatsError::CapacityChanged, VfsError::WouldBlock),
        (
            kirq::IrqStatsError::Overflow,
            VfsError::from(kerrno::LinuxError::EOVERFLOW),
        ),
        (kirq::IrqStatsError::InvalidContext, VfsError::Io),
    ] {
        let file = interrupts_file();
        crate::irq_nodes::interrupts::force_next_stats_error_for_tests(Some(error));
        let mut text = String::new();
        let mut buffer = [0u8; 37];
        let result = loop {
            match file.read(&mut buffer) {
                Ok(0) => break Ok(()),
                Ok(count) => text.push_str(core::str::from_utf8(&buffer[..count]).unwrap()),
                Err(err) => break Err(err),
            }
        };
        crate::irq_nodes::interrupts::force_next_stats_error_for_tests(None);
        assert_eq!(result, Err(expected));
        assert!(text.starts_with("            CPU"));
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn dynamic_record_crosses_initial_buffer() -> unittest::TestResult {
    let line = mapped(0x7409, IrqTrigger::LevelHigh);
    let handler: kirq::Handler = Arc::new(|_| kirq::IrqEvent::HANDLED);
    let name = "012345678901234567890123456789012345678901234567890123456789012";
    for _ in 0..80 {
        assert!(
            line.install_shared_named_action_for_tests(handler.clone(), name)
                .is_some()
        );
    }
    let text = read_all(&interrupts_file());
    assert!(text.len() > 4096);
    assert!(text.contains(name));
    assert!(text.ends_with('\n'));
    TestResult::Ok
}

#[def_test(serial)]
fn seek_end_is_rejected_and_file_is_read_only() -> unittest::TestResult {
    let file = interrupts_file();
    assert_eq!(file.llseek(0, 2), Err(VfsError::InvalidInput));
    assert_eq!(file.node_type(), NodeType::RegularFile);
    assert!(file.write(b"x").is_err());
    TestResult::Ok
}
