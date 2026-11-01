# kcpu_slot 安全说明

完整的 unsafe 类型清单、外部 raw 访问分类和收敛计划见
[`unsafe-convergence.md`](unsafe-convergence.md)。本文保留当前实现必须满足的核心安全
不变量。

`kcpu_slot` 的不安全边界集中在三个位置：架构寄存器读写、模板复制和 slot 指针
运算。

`CpuSlot::with_current_irq_disabled` retains `T: Sync`: masking ordinary IRQs
does not exclude NMI or remote access. It returns no slot borrow and restores
the caller's IRQ mask on return or unwind. Callbacks must not block, schedule,
enable ordinary IRQs, or change the CPU base. It adds no mutable-reference or
pointed-object lifetime guarantee; those remain the payload owner's duty.

调用者必须保证：

1. 模板 LMA 来自与 linker script 同一镜像，`begin_cpu_init` 的目标区域对该 CPU
   唯一、可写，并覆盖 `template_size()` 字节；
2. After the base register is installed, the CPU must not migrate during access. Safe accessors require a `CpuLocalGuard` (currently `kspin::NoPreemptIrqSave`, plus `BootCpuToken` during exclusive CPU bring-up of an unpublished area), which excludes both migration and local IRQ reentrancy;
   `*_unchecked` 入口只要求 `CpuPin`，IRQ 重入排除由调用者建立；
3. Aliasing rules: safe shared and by-value access must not overlap unsafe mutable access. Generic mutable references are exposed through `*_mut_unchecked`, where the caller must prove the object is uniquely borrowed, and through the public unsafe `CpuSlotCell::current_shared_mut`, which returns `&'static mut T` without a pin and requires the caller to be the single execution context on this CPU and not alias the value. Cross-CPU access must not be disguised as local exclusive access; `CpuSlot::at` and `DynamicCpuSlot::get_remote` are available only for `T: Sync`, and unlocked remote writes are not provided by the base API;
4. 链接脚本中的 section 符号和 crate 编译产物来自同一内核镜像，不能由不受信任
   的模块伪造；
5. `CpuSlotChunk::from_raw_parts` 的 backing memory 在所有动态 handle drop 前保持
   有效；`DynamicCpuSlot` 不允许脱离 chunk 生命周期；
6. The static macros `cpu_slot!`/`cpu_slot_cell!` accept only types implementing the public `unsafe trait StaticSlotValue`, while `cpu_slot_cell_with_template!` certifies the exact initializer at the call site. Types that need construction or destruction must not be placed in the template section and must use the per-CPU initializer of a dynamic slot.

架构 base register 是内核特权状态。它只能由平台架构初始化路径安装，不能通过
普通安全 API 任意改写；错误设置会导致任意 slot 访问越界。x86 使用 GS-relative
访问和 GS base 初始化，不假设 FSGSBASE 已启用。未支持架构在非测试构建下无法
编译，避免 base register 访问静默失效。

平台负责发布模板就绪、分配每个 CPU 的 backing area，并在调用 `begin_cpu_init`
后才允许该 CPU 进入依赖 slot 的代码。`kcpu_slot` 不复制 BSP/AP 状态机，也不负责
IRQ、抢占或 CPU online 生命周期。
