// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native code-generation and linker regression checks, not ISA proofs.
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn word(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
}
fn check_architecture(architecture: &str, target: &str) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let directory =
        std::env::temp_dir().join(format!("kcpu-slot-{}-{architecture}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let temporary = Temporary(directory);
    let object = temporary.0.join("architecture.o");
    checked(
        Command::new("rustc")
            .args([
                "+1.95.0",
                "--edition=2024",
                "--crate-type=lib",
                "--emit=obj",
                "-Copt-level=2",
                "--target",
                target,
            ])
            .arg(root.join("architecture.rs"))
            .arg("-o")
            .arg(&object),
    );
    let sysroot = checked(Command::new("rustc").args(["+1.95.0", "--print", "sysroot"]));
    let tools = PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim())
        .join("lib/rustlib/x86_64-unknown-linux-gnu/bin");
    // Resolve real external symbols through each target's linker, rather than
    // letting the assembler fold .equ constants before emitting relocations.
    let production = fs::read_to_string(root.join("../../../linker.lds.S")).unwrap();
    let start = production.find("ASSERT(_cpu_slot_template_size").unwrap();
    let end = production[start..].find(')').unwrap() + start + 1;
    let assertion = &production[start..end];
    let functions = [
        "offset_small",
        "offset_boundary",
        "offset_round",
        "offset_limit",
        "offset_wide",
        "read_base",
        "write_base",
    ];
    let sections = functions
        .iter()
        .map(|name| format!(".text.{name} : {{ *(.text.{name}) }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let linked = temporary.0.join("linked.elf");
    let link = |size: u64| {
        let script = temporary.0.join("link.lds");
        fs::write(
            &script,
            format!(
                "SECTIONS {{ . = 0x100000; {sections} .cpu_slot.template 0 : AT(0x200000) {{ \
                 *(.cpu_slot.template*) }} }}\nOFFSET_SMALL = 0xffff; OFFSET_BOUNDARY = 0x10000; \
                 OFFSET_ROUND = 0x800;\nOFFSET_LIMIT = 0x7ffff000; OFFSET_WIDE = \
                 0x123456789abcdef0;\n_cpu_slot_template_size = {size};\n{assertion}\n"
            ),
        )
        .unwrap();
        Command::new(tools.join("rust-lld"))
            .args(["-flavor", "gnu", "-e", "read_base", "-T"])
            .arg(&script)
            .arg(&object)
            .arg("-o")
            .arg(&linked)
            .output()
            .unwrap()
    };
    let result = link(0x7ffff000);
    assert!(
        result.status.success(),
        "{architecture}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let section = |function: &str| {
        let binary = temporary.0.join(function);
        checked(
            Command::new(tools.join("llvm-objcopy"))
                .args(["-O", "binary", "--only-section"])
                .arg(format!(".text.{function}"))
                .arg(&linked)
                .arg(&binary),
        );
        fs::read(binary).unwrap()
    };
    let mut cases = vec![
        ("offset_small", 0xffffu64),
        ("offset_boundary", 0x10000),
        ("offset_round", 0x800),
        ("offset_limit", 0x7ffff000),
    ];
    if architecture == "aarch64" {
        cases.push(("offset_wide", 0x1234_5678_9abc_def0));
    }
    for (function, expected) in cases {
        let bytes = section(function);
        let actual = match architecture {
            "x86_64" => {
                assert_eq!(bytes.len(), 8);
                assert_eq!(&bytes[..2], &[0x50, 0xb8]);
                assert_eq!(&bytes[6..], &[0x59, 0xc3]);
                u64::from(u32::from_le_bytes(bytes[2..6].try_into().unwrap()))
            }
            "aarch64" => {
                assert_eq!(bytes.len(), 20);
                let mut value = 0u64;
                for index in 0..4 {
                    let instruction = word(&bytes, index);
                    let opcode = if index == 0 { 0xd280_0000 } else { 0xf280_0000 };
                    assert_eq!(instruction & 0xff80_001f, opcode);
                    let shift = ((instruction >> 21) & 3) * 16;
                    value = (value & !(0xffff << shift))
                        | (u64::from((instruction >> 5) & 0xffff) << shift);
                }
                assert_eq!(word(&bytes, 4), 0xd65f_03c0);
                value
            }
            "riscv64" => {
                assert_eq!(bytes.len(), 10);
                let high = word(&bytes, 0);
                let low = word(&bytes, 1);
                assert_eq!(high & 0xfff, 0x537); // LUI a0
                assert_eq!(low & 0xfffff, 0x50513); // ADDI a0,a0
                assert_eq!(&bytes[8..], &[0x82, 0x80]);
                let high_value = (high & 0xfffff000) as i32 as i64;
                let low_value = (low as i32 >> 20) as i64;
                (high_value + low_value) as u64
            }
            "loongarch64" => {
                assert_eq!(bytes.len(), 12);
                let high = word(&bytes, 0);
                let low = word(&bytes, 1);
                assert_eq!(high & 0xfe00_001f, 0x1400_0004); // LU12I.W a0
                assert_eq!(low & 0xffc0_03ff, 0x0380_0084); // ORI a0,a0
                assert_eq!(word(&bytes, 2), 0x4c00_0020);
                let high_value = (((high >> 5) & 0xfffff) << 12) as i32 as i64;
                (high_value as u64) | u64::from((low >> 10) & 0xfff)
            }
            _ => unreachable!(),
        };
        assert_eq!(actual, expected, "{architecture}: {function}");
    }
    let (read, write): (&[u8], &[u8]) = match architecture {
        "x86_64" => (
            &[0x65, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0, 0xc3],
            &[
                0x48, 0x89, 0xf8, 0x48, 0x89, 0xfa, 0x48, 0xc1, 0xea, 0x20, 0xb9, 0x01, 0x01, 0x00,
                0xc0, 0x0f, 0x30, 0x65, 0x48, 0x89, 0x3c, 0x25, 0, 0, 0, 0, 0xc3,
            ],
        ),
        "aarch64" => (
            &[0x80, 0xd0, 0x38, 0xd5, 0xc0, 0x03, 0x5f, 0xd6],
            &[0x80, 0xd0, 0x18, 0xd5, 0xc0, 0x03, 0x5f, 0xd6],
        ),
        "riscv64" => (&[0x0e, 0x85, 0x82, 0x80], &[0xaa, 0x81, 0x82, 0x80]),
        "loongarch64" => (
            &[0xa4, 0x02, 0x15, 0, 0x20, 0, 0, 0x4c],
            &[0x95, 0, 0x15, 0, 0x20, 0, 0, 0x4c],
        ),
        _ => unreachable!(),
    };
    // Pin exact code generation for the designated register. This is not a
    // proof of privileged execution, scheduling, or all possible relocations.
    assert_eq!(section("read_base"), read, "{architecture}: read register");
    assert_eq!(
        section("write_base"),
        write,
        "{architecture}: write register"
    );

    // Exercise the actual production ASSERT immediately above the shared limit.
    let rejected = link(0x7ffff001);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("shared architecture offset range"));
}

#[test]
fn x86_64_contract() {
    check_architecture("x86_64", "x86_64-unknown-none");
}
#[test]
fn aarch64_contract() {
    check_architecture("aarch64", "aarch64-unknown-none-softfloat");
}
#[test]
fn riscv64_contract() {
    check_architecture("riscv64", "riscv64gc-unknown-none-elf");
}
#[test]
fn loongarch64_contract() {
    check_architecture("loongarch64", "loongarch64-unknown-none-softfloat");
}
