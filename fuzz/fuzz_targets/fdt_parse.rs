// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fn explore_fdt(data: &[u8]) {
    let Ok(fdt) = rs_fdtree::LinuxFdt::new(data) else {
        return;
    };

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = fdt.total_size();

        for node in fdt.all_nodes() {
            let _ = node.name;
            let _ = node.compatible();
            for compat in node.compatibles() {
                let _ = compat;
                let _ = node.is_compatible(compat);
            }
            for prop in node.properties() {
                let _ = prop.name;
                let _ = prop.value;
            }
            let _ = node.property("compatible");
            let _ = node.property_str("compatible");
            let _ = node.property_str("model");
            let _ = node.property_u32("reg");
            let _ = node.property_u32("#address-cells");
            let _ = node.parent_property("compatible");
            let _ = node.parent_property_u32("#address-cells");
            let _ = node.cell_sizes();
            if let Some(reg) = node.reg() {
                for region in reg {
                    let _ = (region.starting_address, region.size);
                }
            }
        }

        for reservation in fdt.mem_reservations() {
            let _ = (reservation.starting_address, reservation.size);
        }
        for region in fdt.memory_regions() {
            let _ = (region.starting_address, region.size);
        }
        for region in fdt.reserved_memory_regions() {
            let _ = (region.starting_address, region.size);
        }
        for node in fdt.reserved_memory_nodes() {
            let _ = node.name;
            let _ = node.compatible();
        }

        let _ = fdt.root_model();
        let _ = fdt.root_compatible();
        let _ = fdt.chosen_bootargs();
        let _ = fdt.chosen_stdout_path();
        if let Some(chosen) = fdt.chosen() {
            let _ = chosen.bootargs();
            let _ = chosen.stdout_path();
        }
        if let Some(ic) = fdt.interrupt_controller() {
            let _ = ic.compatible();
        }
        if let Some(dice) = fdt.dice() {
            if let Some(regions) = dice.regions() {
                for region in regions {
                    let _ = (region.starting_address, region.size);
                }
            }
        }

        for path in [
            "/",
            "/chosen",
            "/memory",
            "/soc",
            "/reserved-memory",
            "/aliases",
            "serial0",
        ] {
            let _ = fdt.find_node(path);
            let _ = fdt.resolve_node(path);
        }
    }));
}

fuzz_target!(|data: &[u8]| {
    explore_fdt(data);
});
