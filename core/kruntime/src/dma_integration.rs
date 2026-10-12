// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[kiface::provide]
impl kdma::DmaPageTableIf {
    fn poll_pending_tlb() {
        #[cfg(feature = "ipi")]
        {
            let _guard = kspin::NoPreemptIrqSave::new();
            kipi::tlb::handle_shootdown();
        }
    }

    fn protect(
        vaddr: memaddr::VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> kerrno::KResult {
        let _guard = kspin::NoPreemptIrqSave::new();
        let layout = memspace::kernel_layout();
        loop {
            if let Some(mut locked) = layout.try_lock() {
                return locked.protect(vaddr, size, flags);
            }
            // Another CPU can hold this lock until our IRQ-off CPU acknowledges its shootdown.
            kdma::DmaPageTableIf::poll_pending_tlb();
            core::hint::spin_loop();
        }
    }
}
