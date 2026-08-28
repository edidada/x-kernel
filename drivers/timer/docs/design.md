# timer-driver 设计文档

## 职责

定时器驱动负责架构相关的单调计数器发现、计数器到时间的换算以及时钟事件编程。
平台 crate 提供固件数据或静态回退数据，并在启动期间调用架构后端。
上层通过 `khal::time::MonotonicTimerIf` 使用该实现。

## 组件

- `src/lib.rs` 根据目标架构选择定时器后端。
- `src/arm_generic.rs` 使用 ARM 架构定时器。
- `src/riscv_sbi.rs` 使用 RISC-V 时间计数器和 SBI 定时器服务。
- `src/x86_lapic_tsc.rs` 使用 TSC 作为单调计数器，并使用 LAPIC 定时器提供每 CPU
  一次性时钟事件。

```text
平台启动数据 -> 架构定时器发现 -> 时间换算比例
                                      |
单调时间读取 <-------------------------+
                                      |
定时器期限 -> 架构时钟事件 -> 定时器 IRQ
```

## 执行上下文

架构的 `early_init` 函数在引导阶段、普通任务调度开始前运行。调用上下文需要能够
访问架构计数器以及固件或传统定时器接口，执行过程不依赖当前用户进程。

x86 的 `early_init` 运行在引导处理器上。PIT 校准暂时关闭本地中断，并轮询传统
channel 2，执行有界的 10 ms 单次采样。每次采样记录 PIT 轮询次数以及相邻 TSC
读取之间的最小和最大间隔。物理机使用相邻间隔检查丢弃受到延迟影响的采样；QEMU
TCG 的每次 PIT 读取都会进入设备模型，宿主调度会放大相邻间隔抖动，因此 TCG 只检查
采样在 PIT 到期前至少完成一次轮询并产生 TSC 进度。APIC 完成引导后再进行 LAPIC
初始化。次级处理器只配置各自的 LAPIC 定时器，并复用引导处理器发布的频率。

完成早期初始化后，单调时间读取可以在中断上下文中以非阻塞方式执行。期限编程通过
`x86_apic` 锁访问 CPU 本地 LAPIC 状态，并且要求当前 CPU 的本地 APIC 已完成初始化。

## x86 频率发现

x86 后端按以下来源评估频率：

1. QEMU TCG 优先使用 PIT 校准，因为 TCG 从 QEMU 的 elapsed ticks 派生 guest TSC
   ticks。启用 `-icount` 后，elapsed-tick 来源变为指令计数虚拟时钟。`TCGTCGTCGTCG`
   hypervisor 范围的最高叶为 `0x40000001`，因此 TCG 不枚举 TSC 频率叶。
2. KVM guest 可以通过 QEMU 的 KVM `0x40000010` timing leaf 获得虚拟 TSC kHz，
   ACRN 也定义了同一叶。其他 hypervisor 标识不采用该厂商专用解释。
3. Intel 物理 CPU 和虚拟 CPU 可以通过 CPUID leaf `0x15` 报告 TSC 与晶振的比例。
   Intel CPUID leaf `0x16` 只在比例叶存在时提供处理器基频；驱动先重建晶振频率，
   再应用比例得到 TSC 频率。
4. 枚举信息缺失时使用 PIT 校准提供运行时测量值。
5. 平台静态值作为最终启动回退值。

PIT 校准遵循 Linux 6.8 `arch/x86/kernel/tsc.c` 中的参考计数器结构：channel 2
运行在 mode 0，在已知 PIT 间隔内采样 TSC ticks，并从三次采样中保留最低的有效值，
降低延迟导致的频率膨胀。物理机采样至少包含 1000 次 PIT 轮询，要求相邻 TSC 读取
产生非零进度，并要求最大的相邻间隔最多是最小间隔的十倍。QEMU TCG 采样要求 PIT
到期前至少完成一次轮询并产生非零 TSC 进度，同时跳过相邻间隔比例检查，因为设备
模拟速度和宿主调度会同时破坏物理机的轮询次数及间隔假设。每个轮询循环同时具备
迭代次数上限和基于名义 TSC 频率的截止时间。

相关定义见
[QEMU 8.2 `-icount` 手册](https://qemu.readthedocs.io/en/v8.2.10/system/invocation.html)、
[QEMU TCG CPUID 实现](https://github.com/qemu/qemu/blob/v8.2.2/target/i386/cpu.c)、
[QEMU KVM timing leaf 实现](https://github.com/qemu/qemu/blob/v8.2.2/target/i386/kvm/kvm.c)、
[ACRN CPUID ABI](https://docs.kernel.org/virt/acrn/cpuid.html)以及
[Linux 6.8 TSC 校准](https://github.com/torvalds/linux/blob/v6.8/arch/x86/kernel/tsc.c)。

选定频率以 kHz 保存，因为 `int_ratio::Ratio` 使用 32 位参数。TSC 换算因此保留亚 MHz
精度，并覆盖 x86 实际使用的完整频率范围。

## 并发性

引导阶段一次性初始化 TSC 基线、频率和不可变换算比例。运行时读取只加载原子变量和
不可变比例。LAPIC 状态属于 CPU 本地数据，并由 `x86_apic` 串行化访问。

PIT channel 2 在引导阶段只有一个所有者。校准发生在次级处理器和设备驱动访问传统
定时器端口之前，每次采样期间关闭本地中断。

## 设计决策

- 处理器基频描述处理器工作点，只有在 leaf `0x15` 提供有效比例时才参与 Intel 的
  CPUID 回退计算。
- QEMU TCG 使用实测频率，因为模拟 RDTSC 速率取决于 QEMU 当前的 elapsed-tick 实现。
- 只有 KVM 和 ACRN 使用 hypervisor leaf `0x40000010`，因为 hypervisor CPUID 范围由
  厂商定义，同一个叶编号在其他厂商中可能具有不同含义。
- 物理机依据 Linux 的轮询次数和相邻 TSC 间隔规则丢弃受干扰的 PIT 采样；QEMU TCG
  保留轮询进度检查并接受存在宿主调度抖动的完整样本，然后选择最低的有效值。
- 运行时发现失败时继续使用现有平台值，并在启动日志记录选定来源。
- LAPIC 时钟事件速率保持架构策略常量，与 TSC 频率发现过程分离。

## 资源释放

PIT 校准使用栈上的 guard，在所有返回路径恢复端口 `0x61` 的扬声器和 gate 控制值。
定时器换算比例和每 CPU LAPIC 配置在内核运行期间持续存在。

## 验证

驱动单元测试覆盖来源顺序、厂商范围、频率计算、物理机 PIT 采样质量判定以及 QEMU
TCG 的调度抖动处理，测试不访问真实定时器硬件。外部 Starry Test Harness 的
`ipv4-fragmentation` 用例使用宿主单调时钟
测量 30 秒 IPv4 重组期限。75 秒上限允许 TCG 调度波动，并拒绝因将 1 GHz QEMU TSC
错误选为 4 GHz 平台回退值而产生的四倍期限延长。
