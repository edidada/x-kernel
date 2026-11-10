# driver-init — 设计文档

## 定位

驱动框架的**组合根**（composition root）。全树只有 `core/kruntime` 的主核初始化
调用它一次（`driver_init::init()`），它负责：

1. 安装主机运行时并把 deferred-probe 唤醒钩子接到全局域；
2. 注册所有编译进来的驱动（`module_init` 等价物，一行一个，按 cargo feature 门控）；
3. 把固件设备树、板级静态设备、PCI 层级枚举成 `DeviceDescriptor` 灌进域。

之后一切由 `driver-base` 的域自行推进：探测、defer、唤醒、级联移除。

它是 host-side 树（见 `docs/ai/driver-subsystem-boundaries.md` 的 Scope 表），
因此可以直接依赖 `khal` / `driver-host` / `of` / `pci`。

## 背景

旧框架的等价物是 `drivers/integration/kdriver`（5661 行），它把编排、总线后端、
14 个驱动的 glue、block 完成路径全部装在一个 crate 里。本 crate 是它被拆解后
**剩下的编排部分**——345 行、单文件。拆解的方向是：

- 总线后端 → `bus/pci`（PCI 发现）+ 本 crate 的固件枚举；
- 每个驱动的 glue → `drv/<family>/<module>.rs` 的 `register()`；
- block 完成/回收/IRQ 走线 → `subsystems/block::dispatch`；
- provider 持有 → `drivers/host`。

## 范围

只有一个文件 `src/lib.rs`：

| 部分 | 内容 |
|---|---|
| `init()` | 三段式入口 + 启动末尾的 deferred 报告 |
| `enumerate_firmware_nodes_direct()` | 遍历 `of::fdt()` 的**全部**节点，逐节点构造 `khal::firmware::devices::FirmwareDevice` |
| `build_descriptor()` | `FirmwareDevice` → `DeviceDescriptor`（命名资源表 + 完整 compatible 列表 + provider 属性） |
| `next_firmware_node_id()` | 单调 `AtomicU32` 分配 `DeviceLocation::FirmwareNode{id}`，带耗尽拒绝 |
| `enumerate_static_devices()` | ramdisk、派生时钟、x86 四个 ISA COM 口（带 `IoPortRange`）、板级静态 RTC（`rtc-cmos` / `rtc-ls7a`） |
| `rtc_hctosys()` | probe storm 之后从 `subsys_rtc::RTC` 取采样播种墙钟（Linux `late_initcall(rtc_hctosys)`） |
| `pci_discover()` | 任一 virtio-* 开启时调 `pci::discover::discover(global_domain())` |
| `map_trigger()` | `firmware::InterruptTrigger` → `driver_base::IrqTrigger` |

## 架构

```text
core/kruntime 主核初始化
  workqueue 就绪 → fs_boot::init_device_discovery()
        │
        ▼
  driver_init::init()
        │
        ├─1─ driver_host::Host::install_global()
        │       └─ 域装上 DeviceServices；wake 钩子 = global_domain().wake
        │
        ├─2─ 驱动注册（feature 门控，一行一个）
        │       fixed_clock → pl011 → virtio_blk(先 reclaimer::init)
        │       → virtio_{net,rng,gpu,input,vsock,9p}
        │       → ns16550_{mmio,ioport} → rtc_{pl031,goldfish,cmos,ls7a}
        │       → virtio_mmio → ramdisk
        │
        ├─3─ enumerate_firmware_devices()   // DT 全节点
        │    enumerate_static_devices()     // ramdisk + COM 口 + 板级静态 RTC
        │    pci_discover()                 // PCI 层级
        │       └─ 每次 add_device 都触发一轮 probe_pending
        │
        ├─4─ log 汇总 + deferred_report() + finish_probe_phase(false)
        │
        └─5─ rtc_hctosys()                  // 墙钟播种（RTC 类非空时）
        │
        ▼
  ktty::tty::try_handoff_console() → entropy → init_cb → fb/input → fs → net
```

前一个邻居（`fs_boot::init_device_discovery()`）与后一个邻居
（`try_handoff_console()`）在旧框架下完全相同；重构建后入口是
`driver_init::init()`。

## 调用约束 / 执行上下文

- **只在主核初始化时调用一次**，任务上下文，可睡眠（枚举要分配
  `Vec<String>`、要建 PCI 映射）。
- 必须在 workqueue 就绪之后（`virtio-blk`/`ramdisk` 注册前先
  `block::dispatch::reclaimer::init()`，它建 softirq/worker）、在 console
  handoff 之前（stdout 端口的收养发生在 pl011/ns16550 的 probe 里）。
- `Host::install_global()` 必须先于任何驱动注册：驱动侧 `dev.provide` 走设备
  自己的 `DeviceServices`（未安装时是 `NoServices`，返回 `Unsupported` 而不是
  静默丢弃），HAL/组合根侧的 `Host::global()` 带启动顺序断言。
- 不得在中断上下文调用；不得重入。
- 平台早期部分**不在这里**：两边都是 `khal::early_driver_init()` 在更早处拉起
  timer/IRQ/boot console。

## 算法流程

### 固件枚举：为什么遍历全部节点

旧框架走 `khal::firmware::devices::for_each_compatible(matcher, visitor)`，
按**驱动表反查**兼容节点——只有已注册驱动认识的 compatible 才会变成设备。
本 crate 直接遍历 `of::fdt().all_nodes()`，**每个带 compatible 的节点都变成一个
`DeviceDescriptor`**。

理由：

- 匹配是 `of_device_id` 的 any-match 语义，需要节点的**完整 compatible 列表**
  （`"vendor,soc-uart", "arm,pl011"` 这种"SoC 特化 + 通用 IP"双拼写在 Linux 下
  会被认领）。`for_each_compatible` 的 matcher 只能看到一条，枚举入口就主动
  丢弃了固件数据——这是 review 三轮翻案的一项（§8.3.1）。
- 没有匹配驱动的节点落成 `Unclaimed`：对 introspection 可见、无副作用。
- 顺序无关：`FirmwareNode{id}` 按枚举顺序单调分配，一次启动内稳定。

代价：实测 aarch64 QEMU 上 56 个描述符（旧框架 6 个），每个带
`Vec<String>` compatibles。启动期一次性分配，可接受。

### 资源如何进描述符

| DT 属性 | `ResourceKind` | 命名 |
|---|---|---|
| `reg` + `reg-names` | `Mmio(MmioRegion{base,size})` | 有名字则 `push_named` |
| `interrupts` / `interrupts-extended` + `interrupt-names` | `Irq(IrqSpec::firmware(controller, hwirq, trigger))` | 同上 |
| `clocks` + `clock-names` | `Clk(ClkSpec::by_phandle_index(provider, index))` | 同上 |

**IRQ 保留原始固件路由**：`controller + hwirq + trigger` 原样带走，请求时才经
controller 的 irq domain 解析成 virq（Linux 把 DT spec cells 保留到
`irq_create_of_mapping` 才展开）。压成一个裸全局号只在 domain 映射是恒等的时候
才对——也就是只在 QEMU 上对，这正是 IRQ 三连修的现场（§8.4 教训 3）。

**时钟 provider 节点**（有 `#clock-cells`）额外带 `FirmwareNodeInfo`：
`phandle` / `clock-frequency` / `clock-cells` / `clock-output-name`。
`fixed_clock` 驱动从这里读，不需要重新遍历 DT。

### 启动末尾的 deferred 收尾

```rust
let stuck = domain.deferred_report();
if !stuck.is_empty() {
    for (id, dep) in &stuck { log::warn!("device {id} still deferred on {dep} at boot end"); }
    domain.finish_probe_phase(false);
}
```

即：探测阶段结束时仍 parked 的设备**不再静默 park 下去**，先逐个告警再降级为
`Unclaimed`。`Unclaimed` 不是终态——后注册的驱动或后发布的 provider 仍能唤醒它。

## 并发模型

- `FIRMWARE_NODE_ID: AtomicU32`，`fetch_add(Relaxed)`。单调不回绕；到
  `u32::MAX` 时**拒绝**并 `log::error` 一次，而不是回绕——回绕会让两个设备拿到
  同一个 `DeviceLocation`，从而 devnode 路径、uevent `DEVPATH`、class parentage
  全部撞车。
- 除此之外本 crate 无锁、无共享可变状态。枚举是单线程的（主核初始化）。
- 域自身的并发由 `driver-base` 负责。

## 设计决策

### 组合根只有一个文件、不做抽象

`init()` 里的驱动注册是一串 `#[cfg(feature = "...")] xxx::register();`。
这**看起来**像可以宏化或表驱动，但刻意不做：

- 每一行都是一个可 grep 的 `module_init` 等价物，转写 Linux 驱动时"加一行"是
  机械动作；
- feature 门控在语句级别，宏或表会把 cfg 推进数据结构里，反而更难读；
- 顺序在少数地方是有意义的（`fixed_clock` 先于消费者、`reclaimer::init()` 先于
  `virtio_blk::register()`），显式列出比隐藏在建表逻辑里更安全。

### 枚举代码住在组合根，而不是 `bus-platform` crate

`PlatformBus` 的参考实现在 `driver-base::platform`，枚举逻辑在这里。两处都
标注了"应该在独立的 bus-platform crate 里"。只要枚举只有组合根一个调用者，
拆出去只增加一条依赖边。

### PCI 发现委托给 `bus/pci`

`pci::discover::discover(global_domain())` 自己建 bus instance、自己把 BAR 与
legacy INTx 作为命名资源写进描述符。组合根只决定**要不要**发现（任一 virtio-*
feature 开启时），不决定怎么发现。这是 PCI 成为 driver-base 一等成员的一部分。

## Drop / 资源释放

本 crate 不持有任何资源，没有 Drop 语义。

它分配的东西全部移交给域：`DeviceDescriptor`（含 `Vec<String>` compatibles 与
`ResourceMap`）由域的 `DeviceEntry.desc` 持有，设备移除时随之释放。
`FIRMWARE_NODE_ID` 单调递增不回收——`DeviceLocation::FirmwareNode{id}` 是一次
启动内的稳定身份，回收会让 sysfs 路径在热插拔后改变含义。
