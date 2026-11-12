# kcpu — Design Document

## x86_64 CPU identity and capability snapshots

`src/cpuinfo.rs` owns one `klazy::Once<CpuInfo>` per logical CPU on every architecture;
`src/x86_64/cpuinfo.rs` owns x86 identification and policy. Snapshots are
indexed within `NR_CPUS`. Each BSP/AP calls identification from `init_trap`,
after per-CPU publication and before it can schedule. Initialization must not
migrate. The workspace `raw-cpuid` 11.6 dependency owns vendor/brand assembly,
vendor-specific family/model/stepping decoding and optional-leaf discovery.
Detection accepts a `CpuIdReader`; production uses the native reader, while
tests supply deterministic readers. Identity and the decoded signature are
copied into fixed-size storage; there are no production allocations.

The existing capability words retain leaf 1 EDX/ECX and leaf 7.0 EBX/ECX in
that order. The library's feature objects have no raw-bitmap accessor, so each
supported feature leaf is re-read through the same reader during bring-up.
This preserves unnamed bits and the existing OS state policy without another
per-feature table. The library also probes leaf 7.1, whose flags are unused.
Brand bytes receive a separate printable-ASCII scan before accepting the
library's trimmed string; procfs readers still perform no CPUID queries.

This follows Linux v6.12's separation between per-CPU identification and
capability filtering in [common.c](https://github.com/torvalds/linux/blob/v6.12/arch/x86/kernel/cpu/common.c)
and presentation in [proc.c](https://github.com/torvalds/linux/blob/v6.12/arch/x86/kernel/cpu/proc.c).
`cpu_info(LogicalCpuId)` performs a nonblocking acquire through `Once::get`;
uninitialized/out-of-range CPUs return `None`. Published snapshots are immutable
and are not refreshed by hotplug or microcode updates. Per-CPU flags do not
promise that instructions are usable on every CPU to which a task can migrate.

Only a named subset is exposed through `CpuFeature`; procfs owns Linux flag
spellings. FP/SIMD requires the `fp-simd` build feature, CR4.OSFXSR, CR0.EM clear,
and hardware FPU/FXSR support. SSE-dependent instructions also require SSE/SSE2.
XSAVE/AVX/AVX2/FMA/F16C additionally require the state contract selected by
`xstate.rs`; raw CPUID bits alone never enable their procfs flags.

## Other architectures and adding CPU identification

The common `cpu_info(LogicalCpuId)` API returns the selected architecture's
`CpuInfo`; architecture-specific identity accessors and `CpuFeature` variants
express the native contract without putting Linux presentation strings in kcpu.
The common publication table provides the same nonblocking `None` behavior for
unidentified/out-of-range CPUs on all architectures.

- AArch64 `src/aarch64/cpuinfo.rs` captures MIDR_EL1, ID_AA64PFR0_EL1 and
  ID_AA64ISAR0_EL1 during each CPU's trap initialization. FP/Advanced SIMD and
  dependent crypto require `fp-simd` context preservation. CRC32 and LSE atomics
  are stateless. Reserved feature encodings and SVE are not reported.
- RISC-V `src/riscv/cpuinfo.rs` consumes enabled firmware CPU nodes during
  `init_hwprobe_from_fdt`, sharing the validated raw-to-logical mapping with
  hwprobe. It accepts legacy `riscv,isa` and modern ISA-base/extension properties,
  stores optional firmware architecture IDs, and publishes a selected usable ISA
  subset. Identification can precede AP startup. It does not read machine-mode
  `misa`/identity CSRs from supervisor mode or lazily initialize from procfs.
  Missing firmware leaves the snapshot absent. F/D require both hardware
  extensions and the kernel's double-precision context implementation; V/Zve
  state is not supported. The existing hwprobe ABI and bitmap remain unchanged.
- LoongArch `src/loongarch64/cpuinfo.rs` captures CPUCFG0/1/2 during each CPU's
  trap initialization. CPUCFG provides PRID rather than a marketing model string;
  procfs prints the raw identifier. Scalar FP requires the supported SP/DP state
  format and `fp-simd`; LSX/LASX and binary-translation state are not reported.

To add an architecture, implement its private `cpuinfo.rs`, re-export `CpuInfo`
and `CpuFeature` from the architecture module, and publish through
`crate::cpuinfo` once the identity source and usable-state policy are ready.
CPU-local register detection must run after CPU-area publication, before
scheduling, without migration. Firmware detection must map and validate each CPU
ID before publication. Add synthetic register/firmware tests and extend procfs's
per-architecture formatter and spelling table. Keep hardware filtering in kcpu
and Linux field names in procfs. Update both crates' design/security documents.

These are per-CPU diagnostic snapshots, not Linux's system-wide capability
intersection. AT_HWCAP/AT_HWCAP2 generation and scheduler placement policy remain
separate; this change does not synchronize those interfaces with cpuinfo.

## x86_64 extended state

Following Linux v6.12's [xstate initialization](https://github.com/torvalds/linux/blob/v6.12/arch/x86/kernel/fpu/xstate.c)
and [signal ABI](https://github.com/torvalds/linux/blob/v6.12/arch/x86/kernel/fpu/signal.c), the BSP selects FXSAVE
or standard XSAVE, enabling x87/SSE and, when CPUID leaf 0xD validates the
256-byte component at offset 576, YMM state. Every AP installs the same XCR0
mask before scheduling; an incompatible AP is rejected before tasks can migrate.
AVX-512, AMX and supervisor components remain disabled. No XSAVEOPT/compacted
format is used. `ExtendedState` is an initialized, 832-byte, 64-byte-aligned
object with private headers; task switches eagerly save/restore it with IRQs
disabled. FXSAVE-only CPUs continue to use the first 512 bytes.

Fork/clone captures the creator's live FP registers in `prepare_user_entry` and
restores them on the child's first entry. Exec installs clean x87/SSE/YMM state.
Signal delivery captures fresh storage and exports Linux software metadata,
standard XSAVE header and trailing magic via `mcontext.fpregs`. Handlers start
with clean FP state; nested signals own separate snapshots. The syscall boundary
copies user bytes before `from_signal_bytes` validates length, feature mask,
magic and reserved header words. Legacy signal frames initialize absent YMM
state. Signal handlers may edit supported state; FS/GS bases are not restored
from a private user-writable extension.

`ut-tests/x86_xstate.rs` covers discovery, malformed frames, actual YMM save and
restore, and nested snapshots. The shared Starry harness case
`x86-xstate-semantics` executes AVX2 across concurrent task switches, fork,
exec, nested alternate-stack signals and forged-frame rejection.


## Purpose

`kcpu` provides x-kernel with a multi-architecture abstraction of privileged CPU instructions and structures. It is a core component of the kernel hardware abstraction layer, responsible for trap/exception handling, task context switching, entering/leaving user mode, and safe copying of user-space memory. The scheduler, signal handling, system call dispatch, and driver interrupt handling all depend on this crate.

## Background

The LoongArch kernel IRQ entry also performs the idle return fix-up: if the saved PC points exactly at the `idle` instruction in the dedicated `karch` wait sequence, it is changed to the wait exit address. This prevents an interrupt that arrives after IE is enabled but before `idle` executes from causing the CPU to go back to sleep after the wakeup has been handled. The entry only handles kernel IRQ frames and does not modify user frames or other exceptions; the wait instruction and the fix-up address are owned by `karch`.

x-kernel must run on multiple CPU architectures, and each architecture has a different register layout, exception entry mechanism, context switch method, and set of privileged instructions. This crate encapsulates these differences behind a unified API, so that upper-level kernel code does not need to care about architecture-specific details.

## Scope

Source files involved:

```text
arch/kcpu/
├── src/
│   ├── lib.rs                        # cfg_if architecture dispatch, shared macros
│   ├── excp.rs                       # distributed trap handler registration
│   ├── active_exception_context.rs   # per-CPU current trapframe tracking
│   ├── userspace_common.rs           # ReturnReason, exception table fix-up
│   ├── x86_64/
│   │   ├── mod.rs                    # 导出 ExceptionContext(=TrapFrame)、TaskContext、测试挂载
│   │   ├── xstate.rs                 # 系统保存格式、XSAVE/FXSAVE、Linux 信号 FP 编解码
│   │   ├── cpuinfo.rs                # 每 CPU 身份、受内核状态支持约束的能力快照
│   │   ├── ctx.rs                    # ExceptionContext（含 orig_rax）、ContextSwitchFrame、TaskContext
│   │   ├── excp.rs / excp.S          # trap 入口、IRQ/PageFault 分发、syscall_entry
│   │   ├── instrs.rs / copy_user.S   # user_copy + 异常表条目
│   │   ├── userspace.rs              # UserContext、UserRestorableContext、enter_user、syscall MSR 初始化、syscall_restart_error
│   │   ├── boot.rs                   # init_trap()
│   │   ├── gdt.rs                    # GDT/TSS per-CPU initialization
│   │   └── idt.rs                    # IDT initialization
│   ├── aarch64/
│   │   ├── mod.rs                    # exports, init_trap(), enable_user_timer_access()
│   │   ├── ctx.rs                    # ExceptionContext, FpState, TaskContext
│   │   ├── excp.rs / excp.S          # exception vector table, ESR dispatch
│   │   ├── instrs.rs / copy_user.S   # raw_copy_from_user
│   │   └── userspace.rs              # UserContext, enter_user
│   ├── riscv/
│   │   ├── mod.rs                    # exports, GeneralRegisters
│   │   ├── ctx.rs                    # GeneralRegisters, FpState, ExceptionContext, TaskContext
│   │   ├── excp.rs / excp.S          # scause dispatch
│   │   ├── instrs.rs / copy_user.S   # user_copy
│   │   ├── userspace.rs              # UserContext, enter_user
│   │   ├── boot.rs                   # init_trap()
│   │   ├── hwprobe.rs                # riscv_hwprobe capability snapshot, aggregation, and WHICH_CPUS matching helpers
│   │   └── macros.rs                 # assembly macros (LDR/STR, FP, exception table)
│   └── loongarch64/
│       ├── mod.rs                    # exports, UnalignedError
│       ├── ctx.rs                    # GeneralRegisters, FpuState, ExceptionContext, TaskContext
│       ├── excp.rs / excp.S          # estat dispatch
│       ├── instrs.rs / copy_user.S   # user_copy
│       ├── userspace.rs              # UserContext, enter_user
│       ├── boot.rs                   # init_trap()
│       ├── macros.rs                 # assembly macros (STD/LDD, FP, exception table)
│       └── unaligned.rs / unaligned.S # unaligned access instruction emulation
├── ut-tests/                         # guest tests and helper implementations
├── tests/host/                       # retained cfg(test) host tests
├── Cargo.toml
└── README.md
```

## Test Layout

Guest tests are mounted into their original parent modules through `unittest::path_mod_test!`, and the files are located in `ut-tests/` at the crate root. Module names, visibility, `super` access, and registration paths remain unchanged; the original outer architecture conditions and `cfg(unittest)` are retained, and the macro also provides the guest gate.

| File (relative to `ut-tests/`) | Test cases | Additional gate |
| --- | --- | --- |
| `lib.rs` | 1 | None |
| `excp.rs` | 3 | None |
| `active_exception_context.rs` | 5 | None, all serial |
| `userspace_common.rs` | 3 | None |
| `aarch64.rs` | 7 | AArch64 |
| `aarch64_current_unittest.rs` | 1 | AArch64, existing file changed to path mounting |
| `x86_64.rs` | 6 | x86_64 |
| `x86_cpuinfo.rs` | 13 | x86_64 |
| `x86_xstate.rs` | 3 | x86_64（YMM 用例需要 `fp-simd`） |
| `x86_64_userspace.rs` | 2 | x86_64 and `fp-simd` |
| `riscv.rs` | 4 | riscv32 or riscv64 |
| `riscv_hwprobe.rs` | 7 | riscv32 or riscv64, of which 2 cases are serial |
| `loongarch64.rs` | 5 | LoongArch64 |

实际 guest 用例数量由架构、feature 和测试过滤条件共同决定，以运行时注册清单为准。
`src/aarch64/userspace.rs` 原有 `cfg(test)`
挂载的 `tests/host/aarch64_userspace_host.rs` 保持不变，2 个 host 用例不转换为 guest。

## Architecture

### Layered Structure

```text
                  ┌───────────────────────┐
                  │  Upper-level kernel    │
                  │  (scheduler/signal/syscall) │
                  └───────────┬───────────┘
                              │
              ┌───────────────┼───────────────┐
              │   kcpu unified API             │
              │   ExceptionContext / TrapFrame  │
              │   UserContext / TaskContext     │
              │   init_trap() / user copy       │
              └───────┬───────────────────────┘
                      │ cfg_if dispatch
        ┌─────────────┼─────────────────┐
        │         │         │         │
     x86_64    aarch64    riscv    loongarch64
```

### Shared Layer and Architecture Layer

| Layer | Component | Responsibility |
| ---- | ---- | ---- |
| Shared | `excp.rs` | `IRQ` and `PAGE_FAULT` distributed slices, `handle_trap!` macro |
| Shared | `active_exception_context.rs` | per-CPU current trapframe tracking (`ExceptionContextGuard`) |
| Shared | `userspace_common.rs` | `ReturnReason`, `ExceptionKind`, exception table sorting and fix-up |
| Architecture | `ctx.rs` | `ExceptionContext`, `TaskContext`, FP state |
| Architecture | `excp.rs` + `excp.S` | assembly trap entry → Rust handler |
| Architecture | `instrs.rs` + `copy_user.S` | safe user-space copy + exception table entries |
| Architecture | `userspace.rs` | `UserContext`, `enter_user` assembly call |
| Architecture | `boot.rs` / `mod.rs` | `init_trap()` initialization |

### Current Task Identity

`current_task_ptr()` and `set_current_task_ptr()` are exported by the architecture modules and use an opaque task pointer; they do not depend on `ktask` and do not manage `Arc`. `ktask` is responsible for initialization and for keeping the old and new tasks alive; during normal scheduling, `TaskContext::switch_to(next_ctx, next_task)` publishes current after the hardware context is prepared and before the low-level stack switch. The caller must guarantee that the pointer and the target context belong to the same task. `khal::context` only re-exports the interface and does not keep a second copy of current.

On x86_64, `current.rs` reads and writes this CPU's slot with a single GS instruction. On AArch64, `current.rs` reads the kernel-mode SP_EL0 directly; when publishing, it also updates this CPU's `ENTRY_TASK`, which the EL0 exception entry uses to restore current after saving the user SP. No Rust is called before the entry restore; before returning to EL0, IRQ/NMI are masked first, then SP_EL0 is changed back to the user SP, and afterwards only registers are restored and `eret` is executed. `ENTRY_TASK` is a borrowed pointer and holds no separate task reference; TPIDR_EL0 is still used for user-mode TLS.
On RISC-V, `current.rs` reads and writes the kernel `tp` directly and does not mask IRQ. Boot of the primary and secondary cores first clears `tp`; the user-mode entry saves the kernel current in the kernel callee frame and restores it before returning to Rust, while the user TLS is saved independently by the user trapframe. `gp` still holds the per-CPU base address and does not change with task switches.
Guest entry and exit for the H extension saves and restores the host `tp` in `kvmm`, and does not treat guest x4 as current.
On LoongArch, `current.rs` reads and writes the kernel `$tp` directly and does not mask IRQ. Boot of the primary and secondary cores first clears
this register; the kernel callee frame of the user-mode entry saves current and restores it before returning to Rust,
and the user trapframe saves the user TLS independently. `$r21` continues to hold the per-CPU base address.
Architecture selection happens at compile time,
and upper layers do not branch on the target architecture, nor do they rely on current having to be placed in per-CPU memory.

### Type Relationships

```text
ExceptionContext  (defined per architecture, full-register trap frame)
      │
      ├── alias: TrapFrame = ExceptionContext  (all mod.rs)
      │
      ├── x86_64 extra fields:
      │     orig_rax: original system call number saved at syscall entry
      │              set to u64::MAX (all ones) for non-syscall traps
      │     is_from_syscall() / orig_sysno() query the entry information
      │     rollback_syscall() rolls back rip so the syscall can be re-executed
      │     restart_with_syscall(sysno) restarts with a new system call number
      │
      └── used by the trap handler and UserContext

UserContext       (defined per architecture, wraps ExceptionContext + user-mode-specific fields)
      │           x86_64:      { kernel_rsp, tf: TrapFrame, fs_base, gs_base }
      │                          - kernel_rsp 紧邻 tf，是 enter_user 汇编的私有帧
      │                          - syscall_restart_error() queries the restart error code
      │           aarch64:     { tf: ExceptionContext, sp, tpidr, saved_syscall_arg0 }
      │           riscv:       UserContext(ExceptionContext)     -- newtype
      │           loongarch64: UserContext(ExceptionContext)     -- newtype
      │
      └── Deref/DerefMut → ExceptionContext
          (via the impl_user_context_deref! macro)

UserRestorableContext
      │           x86_64 用于 fork/clone 继承段基址及完整 x87/SSE/YMM 快照，
      │           始终保存在内核；信号使用 Linux ABI 的独立 fpregs 区域。
      │           其他架构携带 ABI `mcontext_t` 未覆盖的窄状态并用于信号帧。
      │           不包含 UserContext::kernel_rsp 私有字段或 syscall restart scratch。

TaskContext       (defined per architecture, callee-saved registers used for context switching)
                  x86_64:      { kstack_top, rsp, cr3, ext_state? }
                  aarch64:     { sp, r19-r29, lr, ttbr0_el1, fp_state? }
                  riscv:       { ra, sp, s0-s11, satp, fp_state? }
                  loongarch64: { ra, sp, s[10], pgdl, fpu? }

                  (fields marked with ? are gated by the fp-simd feature)
```

### Kernel Task Context Construction

`TaskContext::new_kernel(kernel_root)` is the unified construction entry point on all architectures. It explicitly takes
the permanent kernel page table root and does not read the user address space currently active on the creator. This interface only initializes the default
register state; a new task still has its entry point and kernel stack installed by `init()`, while a boot task has its execution state saved by `switch_to()`
on its first switch-out.

- x86_64 / RISC-V store the passed-in permanent root into `cr3` / `satp`.
- On AArch64 / LoongArch, the kernel root has already been installed into a separate register by the boot flow; the constructor keeps
  the user root `ttbr0_el1` / `pgdl` at zero and does not write the passed-in kernel root into the user root field.
- Page table ownership remains with the caller; `kcpu` does not extend the MM lifetime. Before a user task is published, the user address space root held by its runtime must be installed through
  `set_page_table_root()`.

## Calling Constraints / Execution Context

- **`init_trap()`**：必须在 per-CPU 数据初始化之后、调度器启动之前调用。
  每个架构在该函数中完成异常表排序、硬件描述符表加载、trap 向量基址设置。
- **trap handler**（如 `x86_trap_handler`）：运行在中断关闭上下文中，
  不可睡眠或阻塞。由汇编入口直接调用，调用时栈上已保存完整 trap frame。
- **riscv64 `gp` 不变量**：`gp`（x3）是每 CPU 的 percpu 基址，启动时由
  `kcpu_slot` 架构层的 `set_current_base` 设置一次，S-mode trap 期间恒定，故**不**由
  `PUSH_POP_GENERAL_REGS` 保存/恢复——绝不放入可迁移的每任务 trapframe。
  U-mode 用户 `gp` 经 slot-3（`UserContext.regs.gp`）中转：`.Lexit_user`
  入口保存，`.Ltrap_return` 仅在返回 U-mode（SPP==0）时恢复；S-mode 返回
  保持本 hart 的 `gp`。原因：handler 若在 trap 途中阻塞（如 page-fault
  后端），任务会在 PUSH 与 POP 之间迁移到另一 hart，此时从 trapframe 恢复
  旧 hart 的基址会污染 `current()`/`this_cpu_id()`。
- **riscv64 hwprobe snapshot**：`init_hwprobe_from_fdt()` 在平台早期初始化
  从 enabled CPU device-tree node 采集 `riscv,isa` /
  `riscv,isa-extensions`、vendor/arch/impl id、`timebase-frequency`、
  Zicboz block size 和当前用户虚拟地址上限。
  `RiscvHwprobe` private provider 持有 per-CPU capability snapshot；
  对外 helper 接收 Linux raw key，在 `kcpu` 内部完成取值、聚合和
  `WHICH_CPUS` 匹配语义。syscall 路径不读取 M-mode CSR，也不重新解析设备树。
- **`TaskContext::switch_to()`**：unsafe 调度入口，必须由调用者关闭普通 IRQ、
  排除迁移并保留新旧任务。涉及页表切换、FP 状态保存/恢复及 current 发布。
- **`UserContext::run()`**：禁用本地 IRQ 后进入用户态，返回后重新启用。
  调用者必须确保当前处于进程线程上下文且用户页表已正确设置。
- **`user_copy()`**：可在任意内核上下文调用。依赖异常表机制安全处理无效
  用户指针，不会因用户空间地址错误而 panic。
- **`active_exception_context()`**：可在任意上下文调用（包括 NMI 类上下文），
  返回 best-effort 快照。
- **`in_exception_context()`**：在读取 CPU slot 和指针存在性期间屏蔽普通 IRQ，
  随后恢复原屏蔽状态。调用者无需为该布尔快照额外做排除；它不解引用 trap frame，
  不调整抢占计数，也不调用调度器或 RCU 检查。后续队列操作或 reader 移除的保护
  仍由调用者负责。
- **FP/SIMD save/restore**: controlled by the `fp-simd` feature. When it is not enabled, FP state operations are skipped.
- **TLS boundary**: kernel tasks do not allocate TLS and do not switch the TLS register at scheduling time.
  User-mode TLS still belongs to `UserContext` and is saved/restored by the user-mode entry/exit paths.
  The kernel linker script rejects non-empty TLS segments, so that thread-local variables without runtime support do not enter the image.

## Algorithm Flow

### Trap Handling Flow

```text
1. The CPU raises a trap/exception/IRQ
2. Assembly entry (excp.S):
   - save all general-purpose registers to the kernel stack
   - build the ExceptionContext structure on the stack
   - call the architecture-specific Rust handler:
     x86_64:      x86_trap_handler(&mut ExceptionContext)
     aarch64:     dispatch_exception(&mut ExceptionContext, kind, source)
     riscv:       riscv_trap_handler(&mut ExceptionContext)
     loongarch64: loongarch64_trap_handler(&mut ExceptionContext)
3. Rust handler:
   a. create an ExceptionContextGuard (installs the per-CPU active trapframe pointer)
   b. classify the exception by vector number/cause/ESR/estat
   c. Page Fault: read the fault address → try registered handlers → try exception table fixup → panic
   d. IRQ: handle_trap!(IRQ, ...)
   e. Breakpoint: log + advance PC
   f. LoongArch64 unaligned access: emulate_unaligned() instruction emulation
   g. other exceptions: panic with additional diagnostic information
4. Assembly exit:
   - restore registers from the ExceptionContext
   - return from the trap (iretq/eret/sret/ertn)
```

#### AArch64 NMI Entry (pseudo / hardware)

- **pseudo-NMI**: `dispatch_exception` creates a `NmiExceptionGuard` (activated only with the `nmi-pseudo`
  feature and when `karch::pmr::is_ready()`): it stores the entry `ICC_PMR_EL1` in the
  trapframe's `pmr` field and turns on the PMR mask; `use_nmi_path()` classifies an interrupt
  as an NMI when `pmr <= NMI_ONLY`; before returning it first masks IRQs with `daifset`, then restores the entry PMR.
- **hardware NMI**: on exception entry (SPINTMASK=0) the hardware sets `PSTATE.ALLINT=1`, so exception handling
  needs no extra entry code; `use_nmi_path()` classifies with `karch::allint_active() && SPSR.I == 1`
  (an interrupt that still arrives while IRQs are masked must be a Superpriority NMI); the ALLINT window is opened by the GIC IRQ
  dispatch path. `HardwareNmiExitGuard` sets ALLINT again after all handler cleanup is complete,
  covering the early returns of NMIs and exception table fixups, which prevents the return state from being
  overwritten by a nested exception while ELR/SPSR are being restored; `ERET` restores the pre-interrupt state from the unmodified SPSR.
- **runtime degradation**: the entry handling above is all gated on mechanism readiness (`pmr::is_ready()` /
  `allint_active()`); when the mechanism is unavailable (GICv2, no FEAT_NMI) the behavior is exactly
  the same as that of an ordinary IRQ kernel, and NMI-only registers are never touched.

### User-Mode Entry/Exit Flow

```text
UserContext::run(&mut self, on_user_enter, on_user_exit) -> ReturnReason:
1. karch::disable_local_irq()
2. Call on_user_enter, then save the kernel-side FS register value and set the user TLS pointer
3. Call enter_user() [assembly]:
   - Restore user registers from the trap frame
   - Switch to the user page table
   - Return to user mode (sysret/eret/sret/ertn)
4. [User mode executes until a trap/exception/syscall]
5. The assembly trap entry fires, saves registers, and returns to run()
6. Restore the kernel-side FS register value and call on_user_exit (before IRQ dispatch)
7. Classify the return reason:
   - Syscall: specific exception number (int 0x80 / SVC / ecall / syscall)
     x86_64: the syscall_entry assembly pushes rax into the orig_rax field,
     non-syscall traps push -1 (u64::MAX) as a sentinel value
   - Page Fault: read the fault address from the architecture register
   - Interrupt: dispatch the IRQ
   - Other: wrap as ExceptionInfo
8. karch::enable_local_irq()
9. Return ReturnReason

#### x86_64 syscall restart mechanism

A syscall may be interrupted by a signal and need to be restarted. The `orig_rax` field holds the system call number at entry:

- `is_from_syscall()`: checks `orig_rax != u64::MAX` to distinguish a syscall from an ordinary trap
- `rollback_syscall()`: restores `rax = orig_rax` and `rip -= 2` (the length of the syscall instruction)
- `restart_with_syscall(sysno)`: `rax = sysno`, `rip -= 2` (used for restart_syscall)
- `syscall_restart_error()`: checks whether the return value is ERESTARTSYS/ERESTARTNOINTR/
  ERESTARTNOHAND/ERESTART_RESTARTBLOCK, and returns the corresponding LinuxError

Before delivery, the signal handling code calls `syscall_restart_error()` to determine whether a restart is needed,
and then adjusts the user-mode registers through `rollback_syscall()` or `restart_with_syscall()`.
```

### Exception Table Fixup Mechanism

The kernel exception table (the `__ex_table` link section) is used to safely handle the
page faults that occur when kernel mode accesses user-space memory:

1. In the `copy_user.S` assembly, the `_asm_extable` macro generates a
   `{from, to}` mapping entry for each instruction that may fault.
2. At boot, `init_exception_table()` sorts the entries by start address.
3. When a kernel-mode Page Fault occurs, `fixup_exception()` performs a
   binary search on `self.ip()`. On a hit, it sets the IP to the recovery address (`to`) and returns `true`.
4. Effect: the faulting instruction is skipped and the error is returned to the caller of `user_copy()`,
   so the kernel does not panic because of an invalid user pointer.

## Concurrency Model

- **Per-CPU data**: `ACTIVE_EXCEPTION_CONTEXT_PTR`, and the x86_64 `TSS` and `GDT`, are defined through `kcpu_slot`. `GDT` uses `cpu_slot_cell!`, while `TSS` is a hand-written `.cpu_slot.template` symbol wrapped by `CpuSlot::from_offset_fn`. Each CPU has its own copy, so no lock is needed.
- **`ExceptionContextGuard`**: When the trapframe is installed, it saves the original per-CPU slot address,
  and records the previous value with `AtomicUsize::swap`. On Drop, it does not recompute the current CPU; instead it performs
  `compare_exchange(ptr, prev)` restoration on both the original slot and the current CPU slot. This way, even if the
  page fault backend or another trap handler path blocks and migrates, a late guard drop will not
  write the restore to the wrong CPU; if the slot has already been cleared by the scheduler or overwritten by a new trap, the CAS fails and
  the new state is preserved. After migration, when the inner guard is released it must restore the current CPU slot to `prev`, because
  `prev` is the outer live `ExceptionContext` in the nested guard chain of the same task; clearing it would cause the
  second half of the outer trap handler to wrongly pass the `in_exception_context()` check and allow preemption.
- **exception context suspend/restore**: When the scheduler switches away from a task that is still inside a trap handler,
  it suspends and clears the current CPU's active-exception pointer; when that task's context is switched back and returns from
  `switch_to()`, the suspended pointer is restored to the current CPU. This neither leaves a
  stale marker on the old CPU, nor fails to guarantee that `in_exception_context()` is still true when the task continues executing
  the second half of the trap handler after migration.
  `Ordering::Relaxed` is sufficient, because the pointer is only used for best-effort diagnostics and preemption gating,
  and does not publish data across CPUs.
- **Distributed slices**: `IRQ` and `PAGE_FAULT` use `linkme` distributed slices,
  registered at compile time and immutable at runtime, so no synchronization is needed.
- **IRQ-disable protection**: `switch_to()` requires the caller to have already disabled local IRQs; `run()` disables them itself.
  Excluding ordinary IRQs does not mean excluding NMIs; task pointer publication must still guarantee that the object stays alive during an NMI borrow.
- **Naked functions**: every `context_switch` and the AArch64/RISC-V/LoongArch64 FP save/restore helpers use `#[unsafe(naked)]` + `naked_asm!` for full prologue/epilogue control; x86_64 FP state uses the `_fxsave64`/`_fxrstor64` intrinsics, and the LoongArch64 unaligned helpers are `global_asm!` routines from `unaligned.S`.

## Design Decisions

### Why use `cfg_if` rather than a trait object

Each architecture is determined at compile time, and there is no need for runtime switching. `cfg_if` gives zero-overhead dispatch,
with no vtable or indirect call overhead.

### Why `UserContext` exposes `ExceptionContext` through `Deref` rather than inheritance

Rust does not support inheritance. The `impl_user_context_deref!` macro implements
`Deref<Target = ExceptionContext>` for `UserContext`, so that callers can invoke `ExceptionContext` methods directly on `UserContext`,
while the two types can still evolve independently.

### Why the `UserContext` layout differs per architecture

The user-mode switch on each architecture needs different additional state:

- x86_64 needs separate `fs_base` and `gs_base` (MSR switching).
- aarch64 needs a separate `sp` (the EL0 stack pointer is not in the trap frame) and `tpidr`.
- On riscv and loongarch64, `ExceptionContext` already contains the complete state,
  so `UserContext` can simply be a newtype wrapper.

x86_64 的 `UserContext::run()` 把 `&mut UserContext` 直接传给汇编 `enter_user`：
`kernel_rsp` 紧邻 `tf`，汇编把 `&kernel_rsp` 当作私有帧，保存 Rust 调用者的内核栈指针，
并原地使用 `tf` 作为临时 trap stack（TSS.rsp0 指向 `tf` 末尾），避免在用户态窗口前后
拷贝整个 trap frame。因此 `kernel_rsp` 与 `tf` 的相对偏移
（`KERNEL_RSP_TO_TRAPFRAME_OFFSET`）是 `UserContext` 的布局不变量。

信号处理不直接把完整 `UserContext` 放入用户栈。`UContext/MContext` 承载
用户 ABI 寄存器状态，`UserRestorableContext` 补充 ABI 上下文未覆盖的状态
（非 x86 架构的补充状态；x86_64 使用独立 Linux fpregs 区域），避免 x86_64
`UserContext::kernel_rsp` 等内核私有字段泄露到 signal frame 或被 `sigreturn`
恢复。每个信号帧持有独立的 FP 快照，因此嵌套信号也能逐层恢复被中断的 FP/SIMD
状态。

### Why LoongArch64 has software unaligned-access emulation

The LoongArch64 architecture raises an exception on unaligned accesses instead of handling them in hardware. The kernel needs to emulate these accesses
to ensure correctness. `emulate_unaligned()` decodes the opcode of the faulting instruction, determines the operation type
(load/store, size, signed/unsigned, general-purpose/floating-point register), performs the corresponding operation, and then advances the PC.

### Why the user general-purpose registers must be reset after a successful exec

After `execve` successfully loads the new image, the entry user registers must conform to the ELF entry ABI of the target architecture,
and must not inherit the register values from when the old program trapped in a syscall. Each architecture's `UserContext::reset_for_exec()`
is responsible for zeroing all general-purpose registers that should not be inherited as well as the syscall restart related state (`orig_rax`,
`saved_syscall_arg0`, `from_syscall`); the caller then rebuilds the new IP/SP/TLS.

- On x86_64, the third argument `envp` of `execve(path, argv, envp)` is in `rdx`; the `_start` of static
  glibc treats the entry `rdx` as the `rtld_fini` callback and passes it to
  `__libc_start_main`. If it is not zeroed, glibc registers the `envp` address in the old heap as an exit callback,
  and `call *%rax` in `__run_exit_handlers` jumps to the old address, causing SIGSEGV (consistent with the 139 observed by shell
  `echo $?`).
- The other architectures (aarch64/riscv/loongarch64) likewise zero all GPR `x`/`regs`, to prevent old program
  arguments (such as the exec `envp`, which lands in aarch64 `x2` and riscv/loongarch64 `a2`) from leaking into the new
  image, and clear the syscall restart state so that the new program is not treated as a syscall entry.

The reset covers the general-purpose registers and syscall restart state (and, on x86_64, also `gs_base`); frame fields such as `cs/ss/rflags`, `spsr`, `sepc/era/elr`, `sp` and TLS are re-established by the caller via `set_ip`/`set_sp`/`set_tls`.

### Why RISC-V FP state uses lazy save/restore

The RISC-V `sstatus.FS` field tracks the FP state (Off/Initial/Clean/Dirty).
`FpState::switch_to` takes advantage of this mechanism: it saves only when `FS::Dirty`,
and on restore decides, based on the target state, whether to restore, clear, or skip.
This avoids unnecessary FP register operations.

## 析构与资源释放

The types in this crate are mainly register snapshots and context containers, and hold no resources that need explicit release. The RAII guards are `ExceptionContextGuard`, whose `Drop` tries to restore the recorded per-CPU active trapframe pointer to its previous value, plus the feature-gated AArch64 `NmiExceptionGuard` (restores the entry PMR on drop) and `HardwareNmiExitGuard` (re-sets ALLINT on drop).
Before the current CPU switches away from the task that installed this pointer, the scheduler
suspends and clears this CPU's slot, and after the task is switched back it is restored to the new current CPU, avoiding a stale marker on the old CPU
and the absence of the exception context marker in the second half of the trap handler after restoration.

## 中断回调边界

`excp::IRQ` 和 `excp::NMI` 保存 `unsafe fn(usize) -> bool` 回调。架构层负责
在调用 HAL 适配器前持有已捕获的 trap 状态和普通 IRQ 排除；该回调类型不允许
任意安全 Rust 调用冒充中断。内核态与用户态返回路径继续使用各自的入口屏蔽。
普通延迟工作可以在完成中断认领后短暂开启 IRQ，但返回前必须恢复屏蔽状态；
普通适配器负责完成中断后的调度尾部，专用 NMI 回调则必须在不排队的情况下
返回。缺页回调保留其独立的安全签名。
