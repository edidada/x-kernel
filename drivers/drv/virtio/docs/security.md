# drv-virtio — 安全与可靠性分析

## Scope

`drivers/drv/virtio/src/*.rs`（1389 行，驱动模块层）。

设备协议实现与请求队列校验在 `drv/virtio/devices`（包 `virtio-devices`），
它有独立的 `docs/security.md`；本文档只覆盖 driver-core 侧的 probe/remove/
发布/资源所有权，以及本 crate 唯一的一处 `unsafe`。

## 信任模型

virtio 的信任模型与物理设备驱动**根本不同**：设备端是 VMM（ hypervisor 或
QEMU 进程），它对 guest 而言是**半可信**的——它通常是同一个安全域的一部分，
但它控制的 guest 内存范围、描述符内容、used ring 索引都来自 guest 之外。

| 边界 | 谁在外面 | 假设 |
|---|---|---|
| **VMM（virtio 设备端）** | 配置空间、feature bits、队列大小、used ring 的 `idx` 与 `id`、device status | **半可信**。所有从设备读回的索引/长度都必须校验后才用于内存访问。校验在 `virtio-devices`，不在本 crate |
| PCI 配置空间 / DT | vendor/device id、BAR、`virtio,mmio` 节点的 `reg` | 半可信。BAR 由 `bus/pci` 分配与校验；`reg` 由 `memspace` 校验 |
| `driver-host` → 本 crate | `PciTransportService::probe` 的结果、`virtio_hal::Hal` | 可信（同一内核的主机桥） |
| 本 crate → class 消费者 | `publish_block`/`publish_net`/… 的 runtime 对象 | 消费者经 `device_class` 的 `with_available` 门禁访问 |

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| **virtio-mmio 寄存器窗口** | `mmio.rs:107` `probe_mmio_device(regs.as_ptr(), mmio.size)` | 本 crate 唯一的裸指针入口。读 transport header（magic/version/device id）以决定这个槽位有没有设备、是哪一类 |
| **virtio 配置空间** | 经 `PciTransport` / `MmioTransport` → `virtio-devices` | device type、feature bits、queue num_max、config generation |
| **used ring** | `virtio-devices` 的请求完成路径 | `idx`（设备可任意推进）与 `id`（设备可任意指定）——这是 virtio 最主要的内存安全攻击面，校验在 `blk/requests.rs` |
| PCI MSI-X 能力 | `driver-host` → `virtio-devices/src/pci/x86_64.rs` | 见 `driver-host/docs/security.md` T-02 |
| 用户内存 | 间接：块请求的缓冲区、网络 skb、9p/vsock 载荷 | 本 crate 不直接接受用户指针；缓冲区由上层子系统给出，DMA 映射经 `driver-host` |
| 中断 | `dev.request_irq` 的 handler 闭包 | 硬中断上下文，只读设备状态位 |

## unsafe 代码清单

生产代码 **1 处**：

| 位置 | 调用 | 依赖的不变量 | 由谁保证 |
|---|---|---|---|
| `mmio.rs:107` | `virtio_devices::probe_mmio_device(regs.as_ptr(), mmio.size)` | `regs.as_ptr()` 指向一个至少 `mmio.size` 字节的有效映射；探测只**读** header | 紧邻上一行 `let regs = dev.iomap(0)?` 建立了映射，且 `dev.mmio(0)?` 给出的 `mmio.size` 与映射大小一致（`DeviceCtx::iomap` 映射的正是 `mmio(0)` 的 region）。`regs` 在本函数内一直存活，之后经 `ActivationBuilder::new(regs)` 移交 |

**为什么这里必须用裸指针而不是 `IoMap::read32`**：`probe_mmio_device` 在
`virtio-devices` 里，它接受 vendored `virtio-drivers` 的 MMIO 访问形状
（`NonNull<u8>` + size），不知道 `driver_core::IoMap` 的存在。让家族库依赖
`IoMap` 会把 driver-core 的类型渗进 vendored 适配层。

**代价**：这一处访问**不经过 `IoMap` 的边界检查**。缓解在于 `mmio.size` 与
映射大小同源（都来自 `dev.mmio(0)`），且 `probe_mmio_device` 只读 header 的
固定几个偏移。若将来 `probe_mmio_device` 开始按设备返回的长度字段做访问，
这个不变量就不够了——那时必须改成先校验长度。

## 内存安全不变量

1. **`IoMap`（mmio 变体）的生命周期覆盖 transport 的使用期**。映射经
   `ActivationBuilder::take_regs()` 移进设备对象，设备对象被 class 发布持有，
   因此映射活得比发布久 → 消费者 clone 不可能拿着悬垂指针。
2. **IRQ handler 只持 `Weak`**。activation 持强引用，且 teardown 顺序是
   "先撤 IRQ 再销毁 transport"。因此 handler 里 `upgrade()` 返回 `Some`
   就意味着设备仍然完整。
3. **`Activation::close` 幂等**。`Option::take()` 保证 `remove()` 与 `Drop`
   两条路径只有一条真正执行 close。
4. **`BlkActivation::drop` 的顺序是显式代码，不是字段顺序**。
   `begin_close → del_gendisk → drain → disable_interrupts → drop(irq) →
   reclaimer.stop_and_wait → finish_close`。字段 drop 顺序无法表达
   "在两个字段之间等待在途 I/O"，因此 `publication` 字段被显式 `take`。
5. **`remove()` 在 guards 仍存活时运行**（`driver-core` 的契约），因此
   quiesce 阶段可以安全地做 MMIO 与等待。guards 在 `remove()` 返回后才释放。
6. **probe 失败时局部 `activation` 就地 drop**，已获取的 IRQ/发布按逆序释放；
   `XxxData` 从未构造，因此不存在二次释放。

## 线程安全

- 所有 `XxxDriver` 是零大小类型，`Send + Sync`。
- `XxxData` 含 `Option<Box<dyn Activation>>`；`Activation: Send + Sync` 是
  trait 要求，因此 `Data` 满足 `DriverOps::Data: Send + Sync`。
- **IRQ handler 在硬中断上下文**，与任务上下文的 I/O 提交并发。协调在
  `virtio-devices`（队列用 `SpinNoIrq`）与 `block::dispatch`（softirq reclaimer）。
  本 crate 的 handler 只做 `mark_pending()`，把实际回收推到 softirq。
- **reclaimer 的停止与 IRQ 释放有序**：`disable_interrupts()`（设备侧停止产生）
  → `drop(irq)`（主机侧释放）→ `reclaimer.stop_and_wait()`（等已排队的回收跑完）。
  颠倒任何两步都会留下"回收器访问已释放队列"的窗口。
- `blk` 的 `drain.wait_completion()` 会睡眠，因此 close 必须在任务上下文
  （不变量 5 的前提）。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 恶意/故障 VMM 在 used ring 里给出越界的 `id` 或超前的 `idx`，驱动据此访问请求池外的内存 | 高 | VMM 被攻破，或 QEMU 设备模拟 bug | **不在本 crate**。校验在 `virtio-devices/src/blk/requests.rs`（`in_flight: Box<[Option<NonNull<Request>>]>` 的索引检查、`is_empty` 判定），有 6 个 `def_test` 覆盖。**本 crate 的责任**是保证 teardown 顺序不让校验失效（不变量 4） |
| T-02 | `probe_mmio_device` 读到的 header 与真实设备不符（VMM 中途改变 device id），驱动按错误的家族激活 | 中 | 恶意 VMM | **部分实现**：`dispatch` 按 `DeviceKind` 分派，家族 feature 关闭时返回 `Fatal(Unsupported)`；PCI 路径另有 `kind` 校验（`matches!(kind, DeviceKind::Block)`）。**残余风险**：MMIO 路径**没有**二次校验 kind——它直接信任 header。加固建议：在 `dispatch` 之后、`activate` 之内加一次 kind 断言，与 PCI 路径对称 |
| T-03 | `IDS` 表的 transitional id 写错一行，驱动认领了别的家族的设备 | 中 | 转写错误 | **已实现**：`common::pci_transport` 返回的 `kind` 与驱动期望的 `DeviceKind` 不符即 `Fatal(Unsupported)`。`blk.rs` 的 `IDS` 注释明确写了 "0x1000 is *network*——a wrong row here silently steals another driver's device" |
| T-04 | teardown 顺序错误：IRQ 在 transport 销毁之后才释放 → 中断打到已释放的队列 | 高 | 修改 `BlkActivation::drop` 的顺序 | **已实现**：不变量 4 的显式顺序 + `disable_interrupts()` 在 `drop(irq)` **之前**（先让设备停止产生，再释放主机侧处理）。**残余风险**：这个顺序没有测试钉住（remove 路径的测试覆盖是整个框架的已知弱点，见 `docs/driver-framework-review.md` §8.4 教训 2）。加固建议：加一个 mock transport 的 close 顺序断言测试 |
| T-05 | handler 在设备消失后被调用，`Weak::upgrade` 成功但 transport 已销毁 | 高 | IRQ 释放与 transport 销毁的顺序颠倒 | **已实现**：不变量 2 + teardown 顺序（`drop(irq)` 在 `finish_close()` 之前）。`upgrade()` 返回 `None` 时报 `NotHandled`，这是安全默认值（共享线上让别人处理） |
| T-06 | `del_gendisk` 之后仍有在途 I/O 完成，写回已注销的请求 | 高 | drain 缺失或顺序错误 | **已实现**：`begin_close(drain.signals())` 先登记在途请求，`del_gendisk` 撤销设备号，然后 `drain.wait_completion()` 等干净。这正是 `BlockActivation::drop` 里两个 `prepare_block_wait().expect(...)` 的用途（带 `CG-021` 说明：remove 在任务上下文，分配失败不可恢复） |
| T-07 | 消费者持有 `ClassDevice` clone，在设备移除后继续提交 I/O | 高 | 热插拔 | **部分实现**：`device_class` 的数据面走 `with_available` → `acquire_use`，移除编排翻 `draining`/`offline` 后返回 `BadState`。**残余风险**：已过门禁的在途操作与裸 `with()` 调用不受保护，完整闭合需要 per-op 完成计数（`docs/driver-framework-review.md` §8.3 类别 A） |
| T-08 | `mmio` 变体的寄存器映射在最后一个消费者 clone 释放前被 unmap | 高 | 映射所有权放错位置 | **已实现**：不变量 1。映射经 `take_regs()` 移进设备对象，`MmioData::drop` **不**释放它（代码注释明确写了这一点） |
| T-09 | `rng` 的 `HostRngCompletion::wait_until` 在非任务上下文调用 | 中 | 未来某个调用者从中断里读熵 | **部分实现**：`PreparedTaskWait::prepare()` 会在错误上下文失败并返回 `InvalidInput`。**残余风险**：执行上下文前提只写在注释里（例外 E-04 的 rustdoc 要求）。`subsystems/block::dispatch::wait` 有同构的服务化版本可对照 |
| T-10 | 熵源被 VMM 控制（virtio-rng 输出的随机数可预测） | 高 | 恶意 VMM | **不在本 crate**。`KFEAT_ENTROPY_VIRTIO_RNG` 决定是否把 virtio-rng 接入熵池；`util/entropy` 负责混合与可信度标记（启动日志 `hardware_ready=true`）。本 crate 只提供设备驱动 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | transport probe 返回 `Err` | 描述符不是 PCI 位置 / identity 不是 PCI / `PciHost::new` 失败 / vendored 构造器失败 | `RuntimeError` → probe `Fatal` | 设备 `Unclaimed`；其他设备不受影响 | 3 | `common::pci_transport` 把 `RuntimeError` 折成 `Fatal`；MMIO lease 在失败时就地 drop（映射随失败的 probe 消失） |
| F-02 | `probe_mmio_device` 返回 `None`（空槽位） | DT 声明了 `virtio,mmio` 节点但该地址没有设备 | probe `Fatal(BadState)` + `log::debug` | 该槽位不被认领。**这是正常情况**（QEMU 常给出一批空 virtio-mmio 槽），因此只 debug 不 warn | 4 | 刻意选择 `log::debug`：把正常情况记成 warning 会淹没启动日志 |
| F-03 | 家族 feature 关闭但 transport 探测成功 | 配置不完整（例如开了 `virtio-mmio` 没开 `net`） | `dispatch` 返回 `Fatal(Unsupported)` | 该设备 `Unclaimed` | 3 | `scripts/ci/check_feature_matrix.sh` 从 Kconfig 自动派生 `mmio × family` 的全部组合并逐个编译，正是为了这类"合法但从未构建"的组合 |
| F-04 | `request_irq` 失败 | MSI-X 分配失败 / 线冲突 | probe `Fatal`，局部 activation drop | 设备 `Unclaimed`；已发布的 class 条目在 activation drop 时撤销 | 3 | `?` 传播；不变量 6 保证不留半状态 |
| F-05 | `prepare_block_wait()` 在 close 里分配失败 | 内存紧张 | **panic**（`expect`，带 `CG-021`） | 停机 | 2 | 刻意选择：close 运行在任务上下文，无法把错误传回（`Drop` 不返回 `Result`），而"跳过 drain 继续拆设备"会让在途 DMA 写进已释放内存——那比停机更糟 |
| F-06 | `publish_*` 失败（devnode index 耗尽 / 重复发布） | 同一 owner 二次发布，或 class 注册表冲突 | probe `Fatal`，activation drop | 设备 `Unclaimed` | 3 | `map_driver_error` 分类；`device-class` 的注册表查重 |
| F-07 | reclaimer 未初始化就注册 blk 驱动 | `driver-compose` 里 `reclaimer::init()` 的顺序被改 | I/O 完成不被回收 | 块请求永久挂起 | 2 | compose 把两行放在同一个 `#[cfg(feature = "virtio-blk")]` / `ramdisk` 块里。**加固建议**：把 `reclaimer::init()` 收进 `block` 子系统的幂等初始化 |
| F-08 | `rng` 的完成等待丢唤醒 | 设备在 check 与 park 之间 notify | 请求永久挂起 | 熵收集卡住 | 3 | `wait_until` 的循环是"先 register 再 recheck"，与 `block::dispatch::wait` 同构；spurious wake 后重试 |

## 故障管理

- probe 的错误一律经 `common::map_driver_error` 折成 `ProbeError::Fatal`；
  **本 crate 从不手写 `Deferred`**——virtio 设备没有需要等待的 provider
  （transport 由 `driver-host` 同步探测）。
- `remove()` 不返回 `Result`（`DriverOps::remove` 签名如此）。清理失败无法
  上报，因此 close 路径里唯一不可恢复的失败（F-05）选择 panic。
- panic 点 2 处，都在 `BlkActivation::drop` 的 `prepare_block_wait().expect(...)`，
  都带 `CG-021` 例外说明与"任务上下文 + 分配失败不可恢复"的论证。
- `mmio.rs` 的 `probe_mmio_device` 失败不 panic，返回 `Fatal(BadState)`。

## 隐私分析

本 crate 处理**用户数据的载体**但不解析内容：

- `blk`：块请求的缓冲区来自文件系统/页缓存，可能含用户数据。本 crate 只把
  缓冲区交给 transport 做 DMA 映射，不读内容、不落日志。
- `net`：skb 载荷同理。
- `vsock` / `virtio_9p`：guest↔host 通信载荷。**9p 与 vsock 是跨信任域的
  通道**（host 文件系统暴露给 guest，或反之），其协议解析在
  `fs/filesystems/*` 与 `net/knet`，它们必须把对端当不可信。
- `rng`：输出进入熵池。日志里只出现设备名与 index，不出现熵数据。
- `input` / `gpu`：事件与显示内容。input 的 identity（物理位置、unique id）
  会经 devnode properties 进 uevent 广播——见 `devnode/docs/security.md` 的
  隐私分析。

## 已知限制

1. **MMIO 路径不二次校验 device kind**（T-02），PCI 路径有。
2. **teardown 顺序没有测试钉住**（T-04）。这是整个框架的已知弱点
   （remove 路径历史上从未被执行过，见 §8.4 教训 2）。
3. **`probe_mmio_device` 是唯一绕过 `IoMap` 边界检查的访问**，其安全性依赖
   "只读固定偏移的 header"这个当前事实。
4. **MSI-X 只在 x86_64、只对 net 启用**（`devices/src/pci.rs` 的
   `if dev_kind == DeviceKind::Net`）。其他家族用 legacy INTx，直到它们的
   中断行为在共享 queue-vector 下被审计过。
5. **`rng` 的完成等待内联在驱动里**（例外 E-04），未服务化。
6. **`ktask`/`kpoll`/`kirq`/`driver_host` 依赖**（例外 E-02、E-04），
   因此本 crate 不是主机无关的。
7. **无热插拔模板**：框架支持 `add_device` 增量触发，但 T-07 的在途操作窗口
   未闭合，因此不推荐把 virtio 驱动当热插拔参考实现。
8. **`vsock`/`virtio_9p`/`gpu`/`input` 的 `remove` 路径未经验证**（同 T-04）。

## 审计清单

- [ ] 改动 `XxxActivation::drop` 的顺序了吗？不变量 4 的七步是**有序**的，
      特别是 `disable_interrupts` 必须在 `drop(irq)` 之前、`drain` 必须在
      `del_gendisk` 之后。（T-04、T-06）
- [ ] `Data` 新增字段时，是否保持了"发布 guard 在最前"？
- [ ] IRQ handler 闭包是否仍然只捕获 `Weak`？改成 `Arc` 会让 activation 永远
      无法释放（handler 持有强引用，而 handler 由 activation 的 IRQ guard 持有）。
- [ ] 新增家族时是否加了 `DeviceKind` 校验？（T-02、T-03）
- [ ] `IDS` 表的 transitional 行是否核对过类型索引？（0x1000=net、0x1001=blk、
      0x1005=rng；modern = 0x1040 + type）
- [ ] 新增的 `activate_mmio` 是否调用了 `builder.take_regs()` 并把映射交给
      设备对象？漏掉它映射会在 `MmioData::drop` 时被释放，留下悬垂的
      消费者 clone。（T-08）
- [ ] 新的 feature 组合是否被 `check_feature_matrix.sh` 覆盖？（F-03）
      矩阵从 `drivers/Kconfig` 的 `KFEAT_DRIVER_*` 自动派生，所以新增 Kconfig
      符号会自动获得一行——但 arch 绑定的符号需要 `depends on ARCH_*`。
- [ ] close 路径里新增的可失败操作：失败时是 panic 还是忽略？如果忽略，
      是否会导致"跳过 quiesce 继续拆设备"（那比 panic 更糟，见 F-05）？
- [ ] 有没有新增对设备返回的索引/长度的信任？所有这类校验属于
      `virtio-devices`，不要在驱动模块层重复实现。
