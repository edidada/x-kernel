// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[kiface::provide]
impl kdma::DmaPageTableIf {
    fn protect(
        vaddr: memaddr::VirtAddr,
        size: usize,
        flags: khal::paging::MappingFlags,
    ) -> kerrno::KResult {
        let result = memspace::kernel_layout().lock().protect(vaddr, size, flags);
        if result.is_ok() {
            // KernelAddressSpace commits with `finish_no_flush`; release its
            // lock before the global shootdown can send IPIs to CPUs that may
            // be waiting for the same lock.
            memspace::flush_kernel_tlb(None);
        }
        result
    }
}
