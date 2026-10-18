# driver-init — 安全与可靠性分析

## Scope

`drivers/drivers/init/src/lib.rs`（345 行，单文件）。

## 信任模型

组合根是**信任的起点**，不是信任的边界：它决定哪些驱动被注册、哪些固件节点
变成设备。它自己不做安全判断，但它决定了下游谁有机会做判断。

| 边界 | 谁在外面 | 本 crate 假设什么 |
|---|---|---|
| 固件（DT）→ 本 crate | `of::fdt()` 给出的节点、属性、`reg`/`interrupts`/`clocks` | **半可信**。本 crate 只做形状转换与 id 分配，物理地址合法性由 `memspace` 判断，中断号合法性由 `kirq` 判断，时钟 phandle 存在性由 `kclk` 判断 |
| 编译期配置 → 本 crate | `KFEAT_DRIVER_*` 派生的 cargo feature | **可信**。feature 矩阵由 `scripts/ci/check_feature_matrix.sh` 从 Kconfig 自动派生并逐个编译验证 |
| 本 crate → `driver-base` | `add_device(DeviceDescriptor)` | 域会校验父设备与 bus instance；本 crate 不做前置校验 |

## 外部边界 / 攻击面

| 输入源 | 入口 | 说明 |
|---|---|---|
| **设备树** | `of::fdt().all_nodes()` → `build_descriptor` | 主要攻击面。`compatible` 字符串、`reg` 的 base/size、`interrupts` 的 controller+hwirq+trigger、`clocks` 的 phandle+index、`clock-frequency`、`phandle`、`#clock-cells`、`clock-output-names` 全部原样进入描述符 |
| **PCI 配置空间** | `pci::discover::discover` | 委托给 `bus/pci`；本 crate 只决定是否调用 |
| **板级静态表** | `COM_PORTS: [u16; 4]` | 编译期常量，非外部输入 |
| 用户内存 / 用户指针 | 无 | |
| DMA / 中断上下文 | 无 | 本 crate 只在主核初始化的任务上下文运行 |

**关键的"不校验"清单**（每一项都有明确的下游负责者，不是遗漏）：

- `reg` 的 base/size 是否是真实 MMIO、是否与其他设备重叠 → `memspace`
- `hwirq` 是否在该 controller 的合法区间 → `kirq` 的 domain 映射
- `clocks` 的 phandle 是否存在 → `kclk`（不存在则消费者 `Deferred`，不是错误）
- `compatible` 字符串的长度与条数 → **无人校验**（见 T-03）

## unsafe 代码清单

无。本 crate 没有 `unsafe`，不接触裸指针，不做 MMIO。

## 内存安全不变量

1. **`DeviceLocation::FirmwareNode{id}` 在一次启动内唯一**。由单调 `AtomicU32`
   + 耗尽拒绝保证。破坏它的后果不是内存不安全，而是身份混淆：两个设备会得到
   同一个 sysfs 路径、同一个 uevent `DEVPATH`、同一个 class parentage key。
2. **`FirmwareDevice` 借用的 DT 字符串是 `'static`**。DT blob 在启动期被解析后
   常驻，因此 `node.compatibles().collect()` 得到的 `Vec<&'static str>` 可以安全
   转成 `Vec<String>` 存进描述符。若将来 DT blob 变成可释放的，这条不变量会失效。
3. **描述符一旦 `add_device` 就不再被本 crate 触碰**，所有权完整移交给域。

## 线程安全

- 单线程运行（主核初始化），无并发。
- 唯一的共享状态是 `FIRMWARE_NODE_ID: AtomicU32`（`Relaxed` 足够：只需要单调性，
  不需要与其他内存操作排序）。
- `init()` **不可重入**：`Host::install_global()` 会覆盖域的 services
  并重置 `Host`，重复注册驱动会让同一个 `DriverDesc` 进域两次。
  当前靠"只有 kruntime 一个调用点"保证，类型上没有 `Once` 守卫。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | 畸形 DT 给出一个 `reg`，其 base/size 覆盖内核镜像；驱动 `iomap` 后写寄存器 | 高 | 被篡改的 DT | **不在本 crate 防御**。本 crate 原样传递；`memspace::iomap_device` 是唯一校验点（`IoMapError::InvalidRange` → probe `Fatal`）。**残余风险**：见 `driver-host/docs/security.md` T-01 |
| T-02 | 畸形 DT 给出一个不存在的 clock phandle，消费者永久 deferred | 中 | 固件错误 | **已实现**：`driver-base::finish_probe_phase` 在启动末尾报告并降级为 `Unclaimed`；实测日志 `device 70 gave up waiting on dependency clk:66 (provider never registered)`。不会静默卡住 |
| T-03 | 畸形 DT 有海量节点或超长 compatible 字符串，枚举期分配放大 | 低 | 恶意 DT | **部分实现**：`next_firmware_node_id()` 在 u32 耗尽时拒绝并告警一次。**残余风险**：本 crate 不限制节点数、compatible 条数或字符串长度，启动期内存放大无上限。加固建议在 `firmware/of` 解析处限长（记录在 `driver-base/docs/security.md` T-12） |
| T-04 | DT 节点的 `compatible` 首条被当作 `firmware_id`，而驱动匹配用的是完整列表——两者不一致导致 modalias 报错身份 | 中 | 多 compatible 节点 | **已实现**：`firmware_id` 取首条（供诊断与 modalias 主条目），`compatibles` 带完整列表（供 any-match）。`OfMatch::matches` 命中任意一条即可，且 `modalias` 取**得分最高**的那一行的 alias，不是首行 |
| T-05 | 同一 DT 节点被枚举两次（例如 `init()` 被重入） | 中 | 调用点增加 | **未防御**。两个描述符会得到不同的 `FirmwareNode{id}`，因此不会撞身份，但会有两个驱动实例竞争同一份硬件——第二个 probe 会因资源 `Busy` 失败。**加固建议**：给 `init()` 加 `klazy::Once` 守卫 |
| T-06 | 板级静态设备的 `DeviceLocation::PlatformStatic{id}` 与固件节点撞号 | 中 | 新增静态设备时 id 手填 | **部分实现**：ramdisk 用 `id: 0`，四个 COM 口用 `1 + idx`，都在一个 `cfg` 块内相邻可见。**残余风险**：这是手工维护的小整数空间，没有分配器。加固建议：静态设备也走一个单调分配器，或把 id 常量集中到一处 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `of::fdt()` 返回 `None` | 无 DT 的平台（x86 ACPI 启动） | 固件枚举整体跳过 | 只有板级静态设备 + PCI 设备被认领；x86 上这是正常路径 | 4 | `let Some(fdt) = ... else { return }`，不告警（不是错误） |
| F-02 | `add_device` 被拒 | 父设备缺失 / bus instance 类型不符 | 该节点不进入模型 | `log::warn` 后**继续枚举其余节点**——一个坏节点不会中断整个枚举 | 3 | 返回结构化 `AddDeviceError`，四种原因分别可见 |
| F-03 | 固件节点 id 耗尽 | 畸形 DT（>4G 节点，实际不可能） | 后续 DT 设备不再枚举 | 已枚举设备正常工作 | 4 | 拒绝并 `log::error` 一次，不回绕（不变量 1） |
| F-04 | 启动末尾仍有设备 deferred | provider 驱动未编译进来 / phandle 错误 | 这些设备降级为 `Unclaimed` | 逐个 `log::warn` 出来，运维可见 | 3 | `deferred_report()` + `finish_probe_phase(false)` |
| F-05 | 0 个设备 active | `install_global()` 之前有驱动 probe，或 feature 全部关掉 | 无块设备/无网卡 | 启动日志 `driver-base: N firmware descriptor(s), 0 active` 是唯一线索 | 2 | `driver-host` 的 `NoServices` 会返回 `Unsupported` 而不是静默成功，因此 probe 会失败并留下日志。**残余风险**：这条错误的可诊断性依赖运维看启动日志 |
| F-06 | `reclaimer::init()` 未在 `virtio_blk::register()` 之前调用 | 手工调整 `init()` 顺序 | block softirq 回收器不存在 | virtio-blk 的 I/O 完成不会被回收 → 请求永久挂起 | 2 | 当前靠源码里两处相邻的 `#[cfg(feature = "virtio-blk")]` / `ramdisk` 块把两行放在一起。**加固建议**：把 `reclaimer::init()` 收进 `block` 子系统自己的幂等初始化，去掉顺序依赖 |

## 故障管理

- 本 crate 不返回 `Result`：`init()` 是 `pub fn init()`。所有失败都降级为
  "跳过这一个设备 + 日志"，因为启动期一个坏固件节点不该阻止其余设备工作。
- 唯一的 panic 路径是间接的：`driver_host::Host::global()` 在 runtime 未安装时
  panic（HAL/transport 路径使用）。本 crate 的 `init()` 第一行就是
  `Host::install_global()`，所以只要 `init()` 的顺序不变就不可达。
- 日志是主要可观测面：`log::info` 汇总（描述符数 + active 数）、`log::warn`
  逐个被拒节点与卡住的 deferred、`log::error` id 耗尽。

## 隐私分析

无。本 crate 不处理用户数据。日志里出现的是 DT 节点名、compatible 字符串、
物理地址与 phandle——硬件描述，不含用户信息。

## 已知限制

1. **`init()` 无重入守卫**（T-05）。
2. **`PlatformStatic{id}` 是手工维护的小整数空间**（T-06）。
3. **ACPI 设备枚举未实现**。`DiscoveryOrigin::Acpi` 与
   `firmware::FirmwareSource::Acpi` 存在，`khal` 的 `for_each_compatible` 也只
   遍历 DT（其文档写明"ACPI `_HID/_CID` walks can be added here"）。ACPI 目前
   只用于 PCI host/MCFG/IO-APIC 发现。这不是相对旧模型的回退——旧框架同样
   只枚举 DT。
4. **`reclaimer::init()` 的顺序依赖是隐式的**（F-06）。
5. **枚举顺序 = probe 顺序**，而 deferred probe 使这个依赖变弱但没有消失：
   provider 驱动仍需在消费者之前**注册**（不是之前 probe），否则消费者会先
   defer 一轮。当前顺序是手排的。
6. **无 rescan / 热插拔再枚举入口**。`add_device` 是增量的，但没有旧模型
   `DeviceManager::rescan()` 的等价物。

## 审计清单

- [ ] 新增驱动注册是否放在正确的相对顺序上？（provider 先于消费者；
      `reclaimer::init()` 先于 block 驱动注册）
- [ ] 新增的 feature 门控是否在 `api/kfeat/Cargo.toml` 有对应转发，并被
      `scripts/ci/check_feature_matrix.sh` 自动派生覆盖？（矩阵从
      `drivers/Kconfig` 的 `KFEAT_DRIVER_*` 派生，所以新增 Kconfig 符号会自动
      获得一行——但 arch 绑定的符号需要 `depends on ARCH_*` 才能被正确排除）
- [ ] 新增板级静态设备时，`DeviceLocation::PlatformStatic{id}` 是否与现有 id
      冲突？（T-06）
- [ ] 新增的资源类型是否进了 `build_descriptor` 的命名资源表？漏掉会让驱动
      `dev.mmio_by_name("regs")` 静默拿不到资源。
- [ ] 有没有在这里加编排逻辑，而它其实属于 `bus/`、`subsystems/` 或某个
      `drv/<family>`？组合根只负责"注册谁、枚举什么"。
- [ ] `init()` 的调用点是否仍然只有一个？（T-05）
