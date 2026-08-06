// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early-boot display of metadata embedded in the kernel image.

pub(crate) fn print() {
    match kernel_image_metadata::embedded_build_info() {
        Ok(build_info) => kprintln!("{}", build_info.trim_end_matches('\n')),
        Err(error) => kprintln!("build_info = unavailable ({error})"),
    }

    match kernel_image_metadata::embedded_build_id() {
        Ok(build_id) => {
            kprint!("build_id = ");
            for byte in build_id {
                kprint!("{byte:02x}");
            }
            kprintln!();
        }
        Err(error) => kprintln!("build_id = unavailable ({error})"),
    }

    kprintln!("smp = {}", kcpu_id_map::nr_cpus());
    let current_el = karch::current_exception_level();
    kprintln!("current_el = EL{current_el}");
    assert_eq!(current_el, 2, "XHyper runtime must execute at EL2");
}
