// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for aarch64.

use kbuild_config::PSCI_METHOD;
use kplat::boot::{BootHandler, BootInfo};

#[kiface::provide]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &BootInfo) {
        crate::mmio::prepare_boot_memory();
    }

    fn firmware_init(_boot_info: &BootInfo) {
        // PSCI conduit method ("smc"/"hvc") from the /psci FDT node, falling
        // back to the Kconfig default when the node is absent.
        let method = of::find_node("/psci")
            .and_then(|node| node.property_str("method"))
            .unwrap_or(PSCI_METHOD);
        crate::peripherals::psci::init(method);
    }

    fn early_driver_init() {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the required ARM timer \
                      node is missing or invalid; early_driver_init has no error return or \
                      recovery"
        )]
        let timer_config = clocksource_driver::arm_generic::config_from_device_tree()
            .expect("ARM generic timer DT node is required on aarch64");
        clocksource_driver::arm_generic::init(timer_config);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: stdout configuration \
                      cannot be parsed; early_driver_init has no error return or recovery"
        )]
        boot_console::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        irqchip_driver::gic::init_from_device_tree();
        boot_console::register_input_irq_handler();
        #[cfg(feature = "nmi")]
        khal::nmi::early_init();
    }

    fn final_init(_boot_info: &BootInfo) {
        irqchip_driver::gic::init_current_cpu();
        clocksource_driver::arm_generic::init_percpu();
        #[cfg(feature = "nmi")]
        khal::nmi::late_init();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: kcpu_id_map::LogicalCpuId) {
        irqchip_driver::gic::init_current_cpu();
        clocksource_driver::arm_generic::init_percpu();
        #[cfg(feature = "nmi")]
        khal::nmi::late_init();
    }
}
