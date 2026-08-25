// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Panic handler for the runtime.
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("{}", info);
    kprintln!("{}", backtrace::Backtrace::capture());
    // The trace rings are dumped last, and deliberately so: rendering every
    // ring takes seconds on a slow console, and anything that cuts the machine
    // short during it (a watchdog reset, a truncated serial log) must not cost
    // the panic message or the backtrace.
    #[cfg(feature = "lctrace")]
    {
        lctrace::event!(Panic);
        lctrace::dump_to_console(khal::time::freq());
    }
    khal::power::shutdown()
}
