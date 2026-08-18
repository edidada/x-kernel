// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(fdt) = rs_fdtree::LinuxFdt::new(data) else {
        return;
    };

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = fdt.total_size();

        for node in fdt.all_nodes() {
            let _ = node.name;
            let _ = node.compatible();
            for compat in node.compatibles() {
                let _ = fdt.find_compatible(compat);
            }
            for prop in node.properties() {
                let _ = (prop.name, prop.value.len());
            }
            let _ = node.property_str("bootargs");
            let _ = node.property_str("stdout-path");
            let _ = node.reg();
        }

        for region in fdt.memory_regions() {
            let _ = (region.starting_address, region.size);
        }
        for region in fdt.reserved_memory_regions() {
            let _ = (region.starting_address, region.size);
        }
        for reservation in fdt.mem_reservations() {
            let _ = (reservation.starting_address, reservation.size);
        }

        if let Some(chosen) = fdt.chosen() {
            let _ = chosen.bootargs();
            let _ = chosen.stdout_path();
        }
        if let Some(ic) = fdt.interrupt_controller() {
            let _ = ic.compatible();
        }
        if let Some(dice) = fdt.dice() {
            let _ = dice.regions();
        }

        for path in ["/chosen", "/memory", "/soc", "/reserved-memory", "/aliases"] {
            let _ = fdt.find_node(path);
            let _ = fdt.resolve_node(path);
        }

        for node in fdt.all_nodes() {
            if node.name.is_empty() {
                continue;
            }
            let path = format!("/{}", node.name);
            let _ = fdt.find_node(&path);
            let _ = fdt.resolve_node(&path);
        }

        let _ = fdt.chosen_bootargs();
        let _ = fdt.chosen_stdout_path();
        let _ = fdt.root_model();
        let _ = fdt.root_compatible();
    }));
});
