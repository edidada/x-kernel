// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::boot::{BootHandler, BootInfo};

#[kiface::provide]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &BootInfo) {}

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        crate::time::early_init();
        crate::irq::init();
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: console configuration \
                      cannot be parsed; early_driver_init has no error return or recovery"
        )]
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        console_driver::register_input_irq_handler();
    }

    fn final_init(_boot_info: &BootInfo) {
        crate::time::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_logical_cpu_id: kcpu_id_map::LogicalCpuId) {
        crate::time::init_percpu();
    }
}
