# kcpu_slot 安全说明

`kcpu_slot` 的不安全边界集中在三个位置：架构寄存器读写、模板复制和 slot 指针
运算。

调用者必须保证：

1. 模板 LMA 来自与 linker script 同一镜像，`initialize_cpu` 的目标区域对该 CPU
   唯一、可写，并覆盖 `template_size()` 字节；
2. 安装 base register 后，CPU 不在访问期间迁移；`PinCurrentCpu` 的调用者通过
   真实 guard（如 `kspin::NoPreempt`）负责关闭抢占或建立等价的调度器保证；
3. `CpuSlot::get_mut`/`CpuSlotCell::get` 的别名规则由调用者满足，跨 CPU 访问不能
   伪装成本地独占访问；`CpuSlot::get_at` 只在 `T: Sync` 下开放，未持锁远端写
   不在基础 API 中提供；
4. 链接脚本中的 section 符号和 crate 编译产物来自同一内核镜像，不能由不受信任
   的模块伪造；
5. `CpuSlotChunk::from_raw_parts` 的 backing memory 在所有动态 handle drop 前保持
   有效；`DynamicCpuSlot` 不允许脱离 chunk 生命周期；
6. 静态宏只接受 sealed `StaticSlotValue`；需要构造/析构的类型不得放入模板 section，
   必须使用动态 slot 的逐 CPU initializer。

架构 base register 是内核特权状态。它只能由平台架构初始化路径安装，不能通过
普通安全 API 任意改写；错误设置会导致任意 slot 访问越界。x86 使用 GS-relative
访问和 GS base 初始化，不假设 FSGSBASE 已启用。未支持架构在非测试构建下无法
编译，避免 base register 访问静默失效。

平台负责发布模板就绪、分配每个 CPU 的 backing area，并在调用 `initialize_cpu`
后才允许该 CPU 进入依赖 slot 的代码。`kcpu_slot` 不复制 BSP/AP 状态机，也不负责
IRQ、抢占或 CPU online 生命周期。

## 验证边界

[验证清单](../verification/README.md) 区分直接 Kani 证明、平台契约模型下的证明和
宿主/编译/机器指令回归检查。动态内存场景限定 1..=4 个 CPU 及所列类型/分配次数；
backing 的有效性、生命周期、所有权和无并发访问由 harness 建立。构造拒绝参数的
数值检查覆盖完整 usize 域，但不会替调用者证明成功构造所需的有效内存前提。

静态路径的链接器偏移/模板和基址安装/读取由外部模型提供；真实 CPU 固定、IRQ
排他、特权寄存器与 unsafe Sync 的并发语义未被这些模型证明。验收器核对精确模型
替换清单，并单列模型证明数，避免把硬件假设隐藏在总通过数中。

AArch64 原偏移装载在 64 KiB 边界截断，可导致模板复制不足或访问错误槽位；已改为
完整 64 位装载，并增加真实生产宏的交叉编译回归。根链接脚本将四架构模板大小
限制在 0x7ffff000，避免超出共同重定位范围。四架构偏移和基址机器指令、上限及
超限链接结果有自动回归检查；特权执行和寄存器切换协议仍需独立验证，不能据此
宣称跨架构无条件正确。

`stride` 对 usize 顶端输入饱和后向下对齐，结果小于输入，不能用作充足容量。
initializer 返回 Err 的清理已有检查；panic/unwind、任意用户析构器、固件/链接器
和真实并发集成仍在当前证明之外。


平台接入时不得让旧 percpu 与 cpu_slot 以不同布局同时使用同一基址寄存器。
x86_64 需保持 GS/SWAPGS 协议，AArch64 需保持 TPIDR_EL1，RISC-V 64 需在用户态
切换时恢复内核 gp，LoongArch64 需恢复 r21。汇编指令回归只确认库选择了约定寄存器，
不能证明调度器、中断入口和 CPU online 初始化已经满足这些协议。
