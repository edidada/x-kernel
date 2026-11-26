// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;
use kplat::boot::{BootInfo, PlatformBootIf};
use ktime_types::Frequency;
use timer_driver::riscv_sbi::TimerConfig;

/// Timebase of the QEMU `virt` machine, used when the device tree omits it.
const FALLBACK_TIMEBASE: Frequency = Frequency::from_hz(10_000_000);

#[kiface::provide]
impl PlatformBootIf {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        let _ = boot_info;
    }

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        timer_driver::riscv_sbi::init(timer_config());
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: firmware root publication \
                      is required before PLIC discovery; this boot hook has no error return or \
                      recovery"
        )]
        let root = khal::firmware::fwnode_root().expect("firmware root node unavailable");
        irq_driver::riscv::init_primary(root);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the primary PLIC context \
                      must be prepared before local interrupt delivery or runtime readiness"
        )]
        irq_driver::riscv::init_current_cpu_context()
            .expect("primary PLIC context preparation failed");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: stdout configuration \
                      cannot be parsed; early_driver_init has no error return or recovery"
        )]
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        if let Some(fdt) = of::fdt() {
            kcpu::init_hwprobe_from_fdt(fdt);
        }
        #[cfg(feature = "rtc")]
        {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the device-tree RTC \
                          cannot supply the startup wall clock; early_driver_init has no error \
                          return or recovery"
            )]
            let sample =
                rtc_driver::read_from_device_tree().expect("failed to read rtc from device tree");
            ktime::initialize_realtime(sample);
        }
    }

    fn prepare_interrupts(_boot_info: &BootInfo) -> kerrno::KResult<()> {
        // The primary PLIC context was prepared during early discovery.
        Ok(())
    }

    #[cfg(feature = "smp")]
    fn prepare_local_interrupts(_logical_cpu_id: LogicalCpuId) -> kerrno::KResult<()> {
        irq_driver::riscv::init_current_cpu_context().map_err(|error| {
            error!("secondary PLIC context preparation failed: {error}");
            kerrno::KError::BadState
        })
    }

    fn activate_local_interrupts() -> kerrno::KResult<()> {
        irq_driver::riscv::activate_local_interrupts();
        timer_driver::riscv_sbi::init_percpu();
        Ok(())
    }
}

/// Selects the SBI timer configuration, preferring the device-tree timebase.
///
/// Runs before the runtime logger is installed, so the choice is reported on
/// the boot console.
fn timer_config() -> TimerConfig {
    let irq = irq_driver::riscv::S_TIMER;
    if let Some(frequency) = timebase_from_device_tree() {
        kernel_boot::bootln!("RISC-V timebase: {} Hz from device tree", frequency.as_hz());
        TimerConfig::device_tree(irq, frequency)
    } else {
        kernel_boot::bootln!(
            "Warning: /cpus timebase-frequency is missing or invalid; using {} Hz",
            FALLBACK_TIMEBASE.as_hz()
        );
        TimerConfig::platform_static(irq, FALLBACK_TIMEBASE)
    }
}

/// Reads the nonzero `/cpus` `timebase-frequency`, accepting the 4- or
/// 8-byte big-endian encodings that the hwprobe parser accepts.
///
/// A value above `u32::MAX` Hz is returned as is; `riscv_sbi::init` rejects
/// it with a boot panic rather than running with a substituted rate.
fn timebase_from_device_tree() -> Option<Frequency> {
    let value = of::find_node("/cpus")?
        .property("timebase-frequency")?
        .value;
    let hz = match value.len() {
        4 => u64::from(u32::from_be_bytes(value.try_into().ok()?)),
        8 => u64::from_be_bytes(value.try_into().ok()?),
        _ => return None,
    };
    (hz != 0).then_some(Frequency::from_hz(hz))
}
