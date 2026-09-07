# kcpu_slot 验证范围与运行说明

目前有 **32 个 Kani harness**：15 个不替换生产函数，17 个在明确的链接器/寄存器
契约模型下检查 Rust 实现。另有 5 个宿主回归测试（包含四架构检查）、6 个编译失败示例。
**这不是整个库在任意规模、任意调用者和全部架构下无条件正确的证明。**

证明位于 crate 内的独立 Cargo workspace，通过 `kcpu_slot = { path = ".." }`
调用真实生产库。生产 `src/` 不包含证明模块；公共工具仍只有
`scripts/verification/` 下原有的三个 Python 文件，不接 CI。

## 执行与验收

```bash
python3 scripts/verification/run.py arch/kcpu_slot -j 8
# 局部诊断，不算完整验收：
python3 scripts/verification/run.py arch/kcpu_slot --harness stride_contract
```

`cargo-kani`、`cvc5` 需要在 PATH 中；非默认安装需设置 KANI_HOME。固定版本为
Kani 0.67.0、CVC5 1.3.4（git f3b21c4）、宿主 Rust 1.95.0。
四架构检查要求 Rust 1.95.0 的 x86_64-unknown-none、aarch64-unknown-none-softfloat、
riscv64gc-unknown-none-elf、loongarch64-unknown-none-softfloat targets 和 llvm-tools
组件；使用该固定工具链自带的 llvm-objcopy/rust-lld，无额外 GNU Binutils 版本依赖。
工具不全时直接失败。

完整验收检查生产源码指纹、工具版本、23 个公共工具测试、宿主/编译失败测试、
全部 harness 和检查结果、cover、模型替换清单、展开界，以及运行期间输入是否变化。
结果在 `target/kani/kcpu_slot/run.*/acceptance.json`；`full_acceptance: true`
表示清单完整通过，**不表示证明范围没有限制**。`modeled_harnesses` 单列模型证明数。

`config.json` 对每个入口登记准确的 stub 原函数/替代函数配对和 unwind 值。
即使结果显示成功，未登记或缺失的替换、额外 verified stub、缺失元数据都会拒绝。
生产代码变化后应先审查规格，再更新指纹；证明或模型变化后必须重新验收。

## 不替换生产函数的 15 个证明

内存场景使用独占、64 字节对齐、256 字节的 backing，活得比所有 chunk/handle 久。
CPU 数量一般在 1..=4 中符号化选择。合法输入与所有权由 harness 建立，不假设
被测返回值正确；构造参数拒绝域不需要有效 backing，符合生产 rustdoc。

| 入口（dynamic 前缀省略） | 规格和范围 |
| --- | --- |
| `stride_contract` | 全 usize 域：64 字节对齐；可表示时向上取整，差小于 64；顶端 63 个值饱和至 `usize::MAX - 63`，不能算足够容量 |
| `allocation_isolation_and_access` | 每 CPU 64 字节，依次分配 u8/u32/u64；全域 u64 值、任意有效 CPU 的独占修改不影响其他 CPU/槽位；本地读写与远端读取一致；全 usize 越界远端索引返回 None |
| `capacity_is_not_reclaimed_on_drop` | 每 CPU 1..=4 个 u64 的容量；逐次分配/drop，耗尽后 NoSpace、不调用 initializer；drop 不回收 bump 空间 |
| `initialization_failure_cleanup_and_reservation` | 每 CPU 16 字节，任意有效 CPU 返回 Err；按 CPU 顺序初始化，仅析构此前成功对象各一次；失败仍占用预留空间 |
| `successful_handle_drops_each_cpu_once` | 每 CPU 一个 16 字节 Tracked；初始化顺序/次数正确；handle 活着时不析构，正常 drop 后每 CPU 恰好析构一次 |
| `zero_sized_slots_initialize_without_consuming_capacity` | 两个 () 槽位均逐 CPU 初始化，不占后续 u64 容量；不要求零大小对象地址互异 |
| `misaligned_base_does_not_reserve_space` | 一个 CPU，stride 8；base 偏移 1..=7，u64 被拒绝且不初始化；随后 [u8; 8] 可占满区域 |
| `failed_capacity_reservation_preserves_remaining_bytes` | stride 8；u8 成功、u64 对齐后容量不足被拒绝，随后 [u8; 7] 仍能使用剩余空间，原槽位不变 |
| `constructor_rejects_null_all_numeric_inputs` | 空指针，count/stride 遍历全 usize 域，返回 None |
| `constructor_rejects_zero_dimensions` | count 或 stride 为零，另一参数全 usize 域，返回 None |
| `constructor_rejects_total_size_overflow` | count/stride 全 usize 溢出域，以 u128 乘积独立定义域，返回 None；使用 CVC5 避免宽乘法位级求解超时 |
| `misaligned_stride_is_rejected_without_reservation` | stride 1..=64 中非 8 倍数；u64 返回 Misaligned 且不初始化，随后 u8 可正常分配 |
| `local_access_rejects_cpu_outside_chunk` | 机器有 4 个 CPU、chunk 仅有 1..=3 个；有效机器 CPU 在 chunk 范围外时，get/get_mut 返回 None |
| `errors::equality_matches_variant_and_payload` | 三个变体、全 u8 payload 的 Eq/PartialEq 与变体/字段语义一致 |
| `errors::debug_matches_variant_and_u8_payload` | 默认 Debug 精确字节串：NoSpace、Misaligned、Init(0..=255)；不推广到任意 E 或格式选项 |

Tracked 的计数器在全部析构后才销毁；它是代表性析构器，不是任意用户 Drop 的证明。
展开断言保持启用：动态 CPU/分配循环一般用 5，两个数组比较用 9；格式化用 33。
cover 只排除所列边界的空前提，不代表所有组合或接口的测试覆盖率。

## 使用平台契约模型的 17 个证明

`static_slots.rs` 使用真实 `cpu_slot!` / `cpu_slot_cell!` 生成描述符，执行真实的
`initialize_cpu` 复制代码、`area_size` 和全部静态访问器。仅以下边界被替换：

| 边界 | 模型前提，非已证明结论 |
| --- | --- |
| 宏生成的 offset 函数 | 链接器给普通槽位偏移 8、cell 偏移 16 或数组偏移 32；类型对齐、空间和布局相符 |
| `template_size` / 私有 `template_start` | 同一镜像提供正确的模板长度和有效、非重叠源字节；固定模板与宏声明初值一致 |
| 私有 `arch::set_current_base` | 安装指定 CPU 基址，并按 x86 契约写入 self-pointer；不证明 WRMSR/GS 指令 |
| 私有 `arch::current_base` | 返回最近安装的基址；不证明 CPU/IRQ/调度器真的维持该上下文 |

具体入口：

- `initialization_and_static_access_under_platform_contract`：24 字节模板，默认
  `PinCurrentCpu::base` 路径、初始化、get/get_mut/get_at、cell get/read/write；
  u64 写入全域，尾部未复制区域保持原值。
- `static_cpu_areas_remain_isolated_under_platform_contract`：两个独立 CPU 区域，
  修改当前 CPU 不改变另一个 CPU 的普通槽位和 cell 初值。
- 13 个 `static_type_under_platform_contract`：11 种 sealed 标量全部覆盖
  （bool、u8/u16/u32/u64/usize、i8/i16/i32/i64/isize），另加 [u64; 0] 和 [u8; 2]。
  每项检查该类型的普通槽位以及长度 2 数组的 cell，写入值在该类型全域符号化。
- `area_size_under_linker_size_contract`：任意 usize 的链接器大小输入，验证真实
  area_size 对它的饱和对齐计算；不据此宣称真实模板大小汇编正确。
- `template_copy_and_install_order_under_platform_contract`：源长 8..=64，除 x86
  self-pointer 初值为零外其余字节符号化；验证完整复制后才调用基址安装，以及尾部
  不被复制覆盖。安装模型中的断言用于检查调用顺序。展开界为 65。

这些证明没有把 offset 当任意函数传给受限构造器：描述符确由生产宏构造。
模型假设替代链接器/机器执行，必须与独立平台审查一起看，不能叫真实硬件全证明。

## 公有接口核对

| 接口/角色 | 当前证据与限制 |
| --- | --- |
| stride | 全 usize 数值域证明 |
| area_size、template_size、initialize_cpu | Rust 组合/复制行为有条件证明；真实 linker 符号和寄存器未形式化 |
| CpuSlotChunk 的构造、cpu_count、alloc | 拒绝参数的数值域证明 + 上表有限内存场景；非任意历史/大小/类型的全称证明 |
| DynamicCpuSlot 的 get/get_mut/get_remote、Drop | 上表正常/拒绝/失败清理；不覆盖任意 Drop、initializer panic/unwind 或任意并发 |
| CpuSlot/CpuSlotCell 的构造和全部访问方法 | 真实宏生成和真实访问器，在平台契约模型下覆盖 |
| StaticSlotValue | 全部标量实现与代表性数组实例；编译失败示例拒绝拥有资源的类型、引用和外部实现；任意数组长度未逐一证明 |
| SlotInitError | 三变体行为、u8 的 Eq/Debug；任意泛型 E 及格式选项仍依赖其实现 |
| PinCurrentCpu | 测试模型和默认 base 委托；任意 unsafe impl 的真实性是调用者责任 |
| CpuId 重导出 | 使用真实 LogicalCpuId 的 new/as_usize；整个依赖库、固件发现与映射不在本库证明结论中 |
| cpu_slot!/cpu_slot_cell! | 真实宏展开和类型限制；元数据组合和完整链接布局未全称证明 |
| 隐藏偏移宏、架构分支、CpuSlotCell 的 unsafe Sync | 四架构偏移/基址指令有回归；真实特权执行及并发安全仍需独立验证，Sync 编译通过不是并发证明 |

## 发现并修正的规格与实现问题

- AArch64 原偏移宏只用 abs_g0_nc，65536 会变成 0。现在使用四个 16 位装载片段保留
  完整 64 位。`architecture.rs` 直接编译真实生产宏，不复制宏体；AArch64 宿主测试额外核对
  0xffff、0x10000 和 0x123456789abcdef0 三个符号的机器指令和重建结果。
  这是回归检查，不是 AArch64 ISA 或所有重定位的形式化证明。
- 静态描述符可以在编译期建立；初始化要求应在访问前满足，不是在 const 构造时满足。
- 构造拒绝参数不应要求有效 backing；stride 的类型对齐由 alloc 检查。
- 明确 backing 来自单个 allocation 并覆盖全部 handle 生命周期、模板源不重叠、
  静态共享读取不能与写入冲突，以及 stride 饱和结果不能用作足够容量。

## 编译环境和保留工作

Cargo.lock 固定独立 workspace 的真实依赖。`kbuild.rs` 提供 NR_CPUS=4，由公共工具
复制到临时 Kbuild 目录供真实 kbuild_config/build.rs 读取，不使用最近内核构建配置。
Kani 编译器需要 `-Zcrate-attr=feature(cfg_select)`；生产源码不添加该兼容开关。
宿主测试由 Cargo 构建真实依赖；ktime_types 原有直接 rustc 测试路径仍保留。

尚未完成：无界 CPU/分配历史和所有 T 的动态分配归纳证明；全部架构的汇编、寄存器、
链接器/装载器与 ISA 证明；真实抢占/IRQ/CPU online 协议和 unsafe 调用者审计；
panic/unwind、任意析构器/格式化实现，以及验证器模型之外的 Rust 别名语义。
四架构统一要求真实模板大小不超过 0x7ffff000；根链接脚本强制该约束，不能把
AArch64 的完整 64 位装载推广成所有架构支持任意 64 位模板偏移。构建、编译失败测试和源码审查不计为 Kani 证明。
所有结论仍依赖 Kani/CBMC/求解器及其 Rust 模型正确性。


## 四架构检查矩阵

Kani 的 32 项检查仍运行在 x86_64 Rust 模型上，不冒称四种 ISA 的形式化证明。
`architecture.rs` 直接导入生产 macros.rs、arch.rs、layout.rs；
`architecture_checks.rs` 用固定 Rust 工具链分别交叉编译，让目标链接器解析真实外部符号重定位，再检查链接后的机器指令。

| 架构 | 偏移装载 | 基址读写检查 |
| --- | --- | --- |
| x86_64 | 32 位立即数写入 EAX、零扩展 | GS self-pointer 读取；MSR 0xC0000101 的 WRMSR 及 self-pointer 写入 |
| AArch64 | 四段 MOVZ/MOVK，完整 64 位 | TPIDR_EL1 的 MRS/MSR |
| RISC-V 64 | LUI/ADDI，检查低 12 位为负时的进位 | gp 读写；不使用 tp |
| LoongArch64 | LU12I.W/ORI | r21 读写 |

每种架构核对 0x800、0xffff、0x10000、0x7ffff000 四个偏移；AArch64 额外检查
0x123456789abcdef0。解码限于这几个指令序列，只是回归检查，不是通用 ISA 模型。
基址序列检查的是未执行的目标机器码，未证明特权状态或用户态/中断切换行为。

根 linker.lds.S 的 ASSERT 将真实模板大小限制在 0x7ffff000 内，避免有符号或
窄立即数装载超出范围。四个目标的链接器都执行从生产脚本提取的同一条 ASSERT，
分别确认上限通过、上限加一被拒绝。根链接脚本纳入每次验证的输入快照。

系统集成仍须保证每个 CPU 使用匹配的区域，内核/用户态切换正确恢复 GS、
TPIDR_EL1、gp 或 r21，且旧 percpu 布局和新 cpu_slot 布局不能占用同一寄存器却
使用不同基址。当前源码检查未找到生产代码调用 kcpu_slot::initialize_cpu；本轮
交叉编译/链接通过不意味着四个平台已经完成实际初始化接入或运行验证。
