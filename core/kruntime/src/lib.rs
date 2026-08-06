// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime orchestration for x-kernel: primary/secondary CPU bring-up, subsystem
//! init ordering, and handoff to the system-init entry supplied by the `entry`
//! crate.
//!
//! For architecture, boot flow, and security analysis, see `docs/design.md` and
//! `docs/security.md` in the crate source directory.
//!
//! # Cargo Features
//!
//! - `smp`: SMP bring-up (`rust_main_secondary`, `mp`)
//! - `ipi`: Inter-processor interrupts (`kipi`)
//! - `fs` / `fs9p` / `net` / `vsock` / `display` / `input`: driver and subsystem init
//! - `pmu`: PMU overflow IRQ handler
//! - `arm-timer-resume-fixup`: repair virtual timer regressions after idle/WFI
//! - `rootfs-secondary-block`: mount the secondary block device as the root FS
//!
//! All features are optional and disabled by default.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[macro_use]
extern crate klogger;

#[cfg(all(target_os = "none", not(test)))]
mod lang_items;

#[cfg(feature = "smp")]
mod mp;

mod dma_integration;
mod init_setup;

use boot_info::BootConsoleTransport;
use kcpu_id_map::KCpuMaskExt;
use kernel_boot::PrimaryKernelEntry;
#[cfg(not(feature = "smp"))]
use kernel_boot::SecondaryKernelEntry;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr};

#[cfg(feature = "smp")]
pub use self::mp::rust_main_secondary;

const LOGO: &str = r#"
                 ++
             *  ***
           *******
         *******++
      ********+===
    ******#*+======
   *******#***++===
   *********+**===
  ##*********##==                  ====+
%##****++==+****#                ***+=**
#****++=====++****##            ********
####*+=======+******===-     ***#*##****
 #%#******+**+==++**=======+*+*#%
 %#****+******=====+====+++==**
  #*********##***###********##%
   ***##%#%%%%*****#*###*****#%
   *#%%%#%%%%%****   %%*******#%
   #%%%%%%          %%%##***###%%
    **#%           %%%%#% **#**##
   ###%%         %%%%%   %%%***
 %%%%%%        %%%%%%%  %%%%#*
 %%%%%%                %%%%%%+=
"#;

#[kiface::provide]
impl klogger::LoggerAdapter {
    fn write_str(s: &str) {
        khal::console::write_data(s.as_bytes());
    }

    fn now() -> ktime_types::TimeSpan {
        khal::time::monotonic_time().span_since_origin()
    }

    fn cpu_id() -> Option<usize> {
        #[cfg(feature = "smp")]
        {
            is_init_ok().then(|| khal::percpu::this_cpu_id().as_usize())
        }

        #[cfg(not(feature = "smp"))]
        {
            Some(0)
        }
    }

    fn task_id() -> Option<u64> {
        if is_init_ok() {
            ktask::current_may_uninit().map(|curr| curr.owner_key())
        } else {
            None
        }
    }
}

use core::sync::atomic::{AtomicUsize, Ordering};

static INITED_CPUS: AtomicUsize = AtomicUsize::new(0);

/// Hypervisor handoff performed after runtime bring-up.
#[kiface::interface]
pub trait HypervisorRuntimeEntry {
    /// Enters the hypervisor runtime policy layer.
    fn enter();
}

const fn configured_log_level() -> &'static str {
    if kbuild_config::LOG_LEVEL_ERROR {
        "error"
    } else if kbuild_config::LOG_LEVEL_WARN {
        "warn"
    } else if kbuild_config::LOG_LEVEL_INFO {
        "info"
    } else if kbuild_config::LOG_LEVEL_DEBUG {
        "debug"
    } else if kbuild_config::LOG_LEVEL_TRACE {
        "trace"
    } else {
        "off"
    }
}

fn is_init_ok() -> bool {
    // Wait for all *discovered* CPUs (enumerated from the DT/ACPI at boot),
    // not the compile-time `NR_CPUS` cap. Using the cap would deadlock whenever
    // the platform describes fewer CPUs than `NR_CPUS` (e.g. a smaller QEMU
    // `-smp`), because those slots never increment the counter.
    INITED_CPUS.load(Ordering::Acquire) == kcpu_id_map::nr_cpus()
}

/// Runs late subsystem initialization on the bootstrap kernel task.
fn late_init_main(cpu_id: kcpu_id_map::LogicalCpuId) {
    #[cfg(feature = "smp")]
    {
        self::mp::start_secondary_cpus(cpu_id).unwrap_or_else(|err| {
            panic!(
                "failed to start secondary CPUs after boot CPU {} init: {err:?}",
                cpu_id.as_usize()
            )
        });
    }

    kdriver::init_drivers();
    #[cfg(feature = "ipi")]
    kipi::init();

    #[cfg(feature = "smp")]
    kipi::tlb::mark_all_cpus_started();

    init_setup::init_cb();
    finish_allocator_init();
    log_memory_regions();

    info!("Primary CPU {} init OK.", cpu_id.as_usize());
    INITED_CPUS.fetch_add(1, Ordering::Release);

    while !is_init_ok() {
        core::hint::spin_loop();
    }

    HypervisorRuntimeEntry::enter();

    // The late-init thread has finished its job; `task_entry` will exit it.
}

/// Prepares the late-init bootstrap task on the boot CPU.
///
/// Secondary run queues are not registered at activation time, so the thread
/// is pinned to the boot CPU via a one-shot CPU mask.
fn prepare_late_init_on_primary(cpu_id: kcpu_id_map::LogicalCpuId) -> ktask::KtaskRef {
    let task = ktask::TaskInner::new_pidless_kthread(
        move || late_init_main(cpu_id),
        "late_init".into(),
        kbuild_config::TASK_STACK_SIZE,
    );
    let task = ktask::prepare_task(task);
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu_id));
    task
}

fn register_boot_console_runtime_region(boot_info: &boot_info::BootInfo) {
    if boot_info.boot_console_transport != BootConsoleTransport::Mmio {
        return;
    }
    if boot_info.boot_console_addr == 0
        || boot_info.boot_console_vaddr == 0
        || boot_info.boot_console_size == 0
    {
        return;
    }

    let paddr = PhysAddr::from_usize(boot_info.boot_console_addr);
    let vaddr = VirtAddr::from_usize(boot_info.boot_console_vaddr);
    assert_eq!(
        paddr.align_offset_4k(),
        vaddr.align_offset_4k(),
        "boot console MMIO VA/PA offset mismatch"
    );

    let start = paddr.align_down_4k();
    let size = (paddr.align_offset_4k() + boot_info.boot_console_size).align_up(PAGE_SIZE_4K);
    let mapped = vaddr.align_down_4k();
    memspace::register_fixed_device_region(start, size, "boot-uart", mapped)
        .expect("failed to register boot console runtime region");
}

/// The main entry point of the runtime.
///
/// It is called from the bootstrapping code in the specific platform crate (see
/// `kplat::main`).
///
/// `arg` is the unified bootloader handoff payload (`BootInfo*`) for the
/// primary CPU. Secondary cores call [`rust_main_secondary`].
pub fn rust_main(arg: usize) -> ! {
    let boot_info = khal::boot_info(arg);
    let cpu_id = boot_info.cpu_id;

    kernel_boot::bootln!(
        "kruntime primary start cpu={} boot_info={arg:#x}",
        cpu_id.as_usize()
    );
    khal::firmware::init(boot_info);
    khal::percpu::init_primary(cpu_id);
    kcpu::init_trap();
    khal::mem::init(boot_info);
    kernel_boot::bootln!("kruntime memory regions ready");
    init_allocator();
    kernel_boot::bootln!("kruntime allocator ready");
    register_boot_console_runtime_region(boot_info);
    memspace::init_memory_management();
    kernel_boot::bootln!("memory space map ready");
    // Install the OS-agnostic resource provider before any driver (including
    // early device interrupts like the console input line) acquires a resource.
    kdriver::install_resource_provider();
    khal::early_driver_init();

    kprintln!("{}", LOGO);
    klogger::init_klogger();
    klogger::set_log_level(configured_log_level()); // no effect if set `log-level-*` features
    info!("Logging is enabled.");
    info!(
        "Primary CPU {} started, boot_info = {arg:#x}.",
        cpu_id.as_usize()
    );
    {
        use core::ops::Range;

        // SAFETY: The `_stext` and `_etext` symbols are guaranteed to be valid by the linker.
        unsafe extern "C" {
            safe static _stext: [u8; 0];
            safe static _etext: [u8; 0];
        }

        let ip_range = Range {
            start: _stext.as_ptr() as usize,
            end: _etext.as_ptr() as usize,
        };

        // fp_range must cover both:
        //   - KIMAGE_VADDR stacks (primary CPU task stacks, linked at KIMAGE_VADDR)
        //   - Linear-map stacks (secondary CPU boot stacks, at PA + PAGE_OFFSET)
        // So start from PAGE_OFFSET rather than _edata.
        let fp_range = Range {
            start: kaddr_layout::PAGE_OFFSET,
            end: usize::MAX,
        };

        backtrace::init(ip_range, fp_range);
    }

    info!("Initialize platform devices...");
    khal::final_init(boot_info);

    ktask::init_scheduler();
    info!("Initialize interrupt handlers...");
    init_interrupt();

    // Activate a single late-init bootstrap task that brings up late
    // subsystems and enters the hypervisor runtime. Pinning it to the boot CPU
    // keeps it independent of secondary run queues, which are not registered
    // yet.
    let late_init = prepare_late_init_on_primary(cpu_id);
    ktask::activate_task(&late_init);
    info!("Started late-init bootstrap thread.");

    // Once the late-init task is runnable, the boot task has no further role.
    ktask::future::block_on(core::future::pending::<()>());
    unreachable!("boot task must remain parked");
}

#[kiface::provide]
impl PrimaryKernelEntry {
    fn enter(boot_info: usize) -> ! {
        rust_main(boot_info)
    }
}

#[cfg(not(feature = "smp"))]
#[kiface::provide]
impl SecondaryKernelEntry {
    fn enter(_logical_cpu_id: kcpu_id_map::LogicalCpuId) -> ! {
        panic!("secondary CPU entered a uniprocessor kernel")
    }
}

fn log_memory_regions() {
    fn log_region(region: &khal::mem::MemoryRegion) {
        let reserved_source = if region.flags.contains(khal::mem::MemFlags::RSVD) {
            khal::mem::reserved::describe_reserved_memory_region(region)
                .map(|reserved| reserved.source)
        } else {
            None
        };
        if let Some(source) = reserved_source {
            info!("  {} [source={}]", region, source);
        } else {
            info!("  {}", region);
        }
    }

    fn log_device_region(region: &memspace::DeviceRegion) {
        if let Some(vaddr) = region.vaddr {
            info!(
                "  [PA:{:#x}, PA:{:#x}) [VA:{:#x}, VA:{:#x}) {}",
                region.paddr.as_usize(),
                region.paddr.as_usize() + region.size,
                vaddr.as_usize(),
                vaddr.as_usize() + region.size,
                region.name,
            );
        } else {
            info!(
                "  [PA:{:#x}, PA:{:#x}) {}",
                region.paddr.as_usize(),
                region.paddr.as_usize() + region.size,
                region.name,
            );
        }
    }

    info!("Found physcial memory regions:");
    for region in khal::mem::memory_regions() {
        log_region(&region);
    }

    info!("Found runtime device/iomap regions:");
    for region in memspace::device_regions() {
        log_device_region(&region);
    }
}

fn init_allocator() {
    use khal::mem::{MemFlags, memory_regions, p2v};

    info!("Initialize global memory allocator...");
    info!("  use {} allocator.", kalloc::global_allocator().name());
    let mut free_regions = memory_regions().filter(|r| r.flags.contains(MemFlags::FREE));
    if let Some(region) = free_regions.next() {
        kalloc::global_init(p2v(region.paddr).as_usize(), region.size);
    }
    for region in free_regions {
        kalloc::global_add_memory(p2v(region.paddr).as_usize(), region.size)
            .expect("failed to add free region to allocator");
    }
}

fn finish_allocator_init() {
    let allocator = kalloc::global_allocator();
    info!(
        "Allocator state: heap used={:#x}, heap avail={:#x}, pages used={}, pages avail={}, \
         usages={:?}",
        allocator.used_bytes(),
        allocator.available_bytes(),
        allocator.used_pages(),
        allocator.available_pages(),
        allocator.usages()
    );
}

fn init_interrupt() {
    // Timer interrupt handler. All hardware-timer driving — periodic-tick
    // bookkeeping, soft-timer wheel drain, and hardware re-arm — lives in
    // `ktask::on_timer_fire`.
    khal::irq::register(
        khal::time::interrupt_id(),
        alloc::sync::Arc::new(|| {
            ktask::on_timer_fire();
            khal::irq::IrqEvent::HANDLED
        }),
    );

    #[cfg(feature = "ipi")]
    khal::irq::register(
        kbuild_config::IPI_IRQ,
        alloc::sync::Arc::new(|| {
            #[cfg(feature = "arm-timer-resume-fixup")]
            timer_driver::arm_generic::handle_ipi_fixup();
            #[cfg(feature = "ipi")]
            kipi::ipi_handler();
            khal::irq::IrqEvent::HANDLED
        }),
    );

    #[cfg(feature = "pmu")]
    khal::irq::register_nmi(
        khal::irq::gic_level_irq_desc(kbuild_config::PMU_IRQ),
        alloc::sync::Arc::new(|| {
            debug!(
                "PMU interrupt received on cpu {}",
                khal::percpu::this_cpu_id().as_usize()
            );
            khal::pmu::dispatch_irq_overflows();
            khal::irq::IrqEvent::HANDLED
        }),
    );

    // Enable IRQs before starting app
    karch::enable_local_irq();
}
