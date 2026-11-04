# 平台 / VirtIO-MMIO 枚举迁移：范围评审稿

状态：现状盘点与范围建议，待用户评审；不是已批准设计或开发 tickets。
基线：`3cee34dcbc9f9ba4231edb43287439785149bacf`，2026-11-04。
本轮只读源码并整理文档，未修改生产代码、Kconfig、defconfig 或运行构建/QEMU。
此前 R1-01～09 的验收不能替代本阶段未来实现的验收。

## 1. 用户确认的方向

- 迁移整条平台 / VirtIO-MMIO 枚举路径，不把 RNG 专用分流留作最终架构。
- 新旧路径清楚分离、分 PR 合入；先需求与范围，再设计评审、tickets、开发。
- 最终设备统一进入新模型，有没有驱动不决定有效设备能不能注册。
- 网络生命周期问题随网络驱动正式迁移一起设计、实现、验收。

下面的配置选择、迁移分组和例外处理仍是建议，不以本文替用户批准。

## 2. 现有入口和实际管理者

```text
kruntime 初始化服务/工作队列
  → fs_boot 准备设备发现订阅
  → kdriver::init_drivers
      → DeviceManager / default_bus_manager
          → 旧平台 bus + PlatformBackend
          → 旧 PCI bus + PciBackend
      → 注册旧 driver factories
      → 枚举（持 bus manager 锁）
          → 旧 DeviceDesc pending
          → 特选 RNG 的 StagedRng pending
      → 离锁 probe_pending
          → 先处理新 domain 注册 + probe
          → 再处理旧 kdevice probe
  → TTY 接管 / entropy / 文件系统 / 网络等消费者初始化
```

源码入口：`core/kruntime/src/lib.rs:385`、
`drivers/integration/kdriver/src/lib.rs:142`、`src/manager.rs:27`、
`src/bus/manager.rs:117`、`src/enumeration.rs:150`（后三处相对 kdriver）。

关键事实：

1. `BusManager::register` 直接创建旧 kdevice bus；当前并非可直接换一个
   enumerator 就得到新注册所有者。见 `src/bus/manager.rs:30`。
2. `PlatformBackend::enumerate_firmware` 的普通设备发现依赖旧已注册驱动的
   firmware match specs，VirtIO 是特判。见 `src/bus/platform_backend.rs:61`。
3. 当前 `khal::firmware::devices::for_each_compatible_view` 实际只遍历 DT，
   无 FDT 便返回；不能根据上层“DT/ACPI”注释声称通用 ACPI 平台枚举已实现。
   当前视图仅取首个 reg / interrupt，需在设计中明确支持范围，不能把所有
   节点一股脑塞入同一种设备描述。见 `arch/khal/src/firmware/devices.rs:325`。
4. modern-MMIO RNG 在旧注册前特选进入新模型；PlatformModel 还安装 native
   services、创建根 bus 并注册 RNG driver。它不是通用平台枚举模块。
   见 `src/bus/platform_backend.rs:156`、`src/platform_model.rs:89`。
5. `probe_pending` 中新模型注册可能同步 probe；新入口仍须保留发现锁外执行
   回调的约束。现有统计只覆盖部分旧路径，不能拿旧 activated/descriptor
   计数证明新路径完整。见 `src/enumeration.rs:150`、`src/manager.rs:167`。

## 3. 设备与驱动消费者清单

以下是源码支持面，不代表某一 defconfig 启用或 QEMU 实际挂载了全部设备。

| 组 | 当前发现 / 驱动入口 | 当前所有者及迁移注意事项 |
|---|---|---|
| PL011 串口 | 固件匹配；`driver_registry/char/serial.rs:140` | 旧 kdevice；stdout 尽量接管 early SerialPort，不能重建同一硬件所有者 |
| NS16550 MMIO 串口 | 固件匹配；同文件 `:181` | 旧 kdevice；保留 reg shift/width 与 early port 接管 |
| NS16550 I/O 串口 | 静态 alias；同文件 `:224` | 旧 kdevice；x86 固定端口接管，不应变成伪 MMIO |
| ramdisk / 静态 ramdisk | 静态 alias；`driver_registry/block/ramdisk.rs:14` | 旧 kdevice + block 发布；可能承担真实根文件系统，不可视为无关测试设备 |
| AHCI | 固件匹配，或无固件时静态地址 | 旧 kdevice；资源、块发布和退出协议需逐项迁移 |
| BCM2835 SDHCI | 固件匹配，或无固件时静态地址 | 同上，不能仅改 register 调用 |
| SDMMC | 固件匹配，或无固件时静态地址 | 同上 |
| FXMAC | 固件匹配，或无固件时静态地址 | 旧 kdevice + net 发布；归网络迁移评审，不默认已有安全 close |
| VirtIO-MMIO RNG | 特选 QEMU Arm modern 路径新模型；其他仍旧路径 | 已有 RNG 运行/退出可复用，但当前不是 transport → VirtIO child 分层 |
| VirtIO-MMIO block | transport 类型判别后旧 VirtioDriver | 已有显式 block close 入口，仍需验证新所有权接线，不能直接声称迁移完成 |
| VirtIO-MMIO net | 同上 | 旧 remove 没有对应显式 net close；保留网络迁移任务 |
| VirtIO-MMIO GPU / input | 同上 | 保留 display/input、DeviceNode 与 sysfs/uevent 的现有关系 |
| VirtIO-MMIO vsock / 9p | 同上 | 对应 socket / host share 消费者；逐家族确认退出，不能从共同构造函数推导共同生命周期 |
| PCI host（特殊） | **PCI backend 自行 adopt**，不经 PlatformBackend 的普通匹配 | 在旧平台 bus 中是 platform device，仍是旧 PCI 端点的父对象 |
| PCI endpoints / bridges | PCI backend | 本阶段不默认迁移；ixgbe、PCI VirtIO 等保持原路径和兼容回归 |

静态清单：`drivers/integration/kdriver/src/bus/platform_backend.rs:280`。
固件匹配：`src/driver_registry/firmware_specs.rs:20` 与 char serial 的 match specs。
VirtIO factories 同时注册各启用家族的 PCI/MMIO driver，并非仅按 QEMU transport
选择编译一个：`src/driver_registry/virtio/mod.rs:374`。
VirtioDriver::remove 当前仅显式 close block：同文件 `:77`。
这里没有把其他驱动继承的默认 remove 当作安全停机证据。

### 启动基础设施不是普通待迁移设备

早期 console、irqchip、timer 以及固件提供者有独立启动职责；不能通过“遍历所有
compatible 节点”重复接管这些硬件。early console 的后续 TTY 交接属于本阶段
要保留的接口，但不因此重写早期启动。
参考 `drivers/platform/console/src/lib.rs:5`、
`drivers/integration/kdriver/src/driver_registry/char/serial.rs:77`、
`core/kruntime/src/lib.rs:547`。

## 4. 发现的范围例外：PCI host

`PciBackend` 将 host 直接 adopt 到名为 `platform` 的旧 bus，再把它作为 PCI
端点的父对象 / bus controller；找不到旧平台 bus 时会降级为无父端点布局。
见 `drivers/integration/kdriver/src/bus/pci_backend.rs:195`。

因此“本轮不迁移 PCI”与“立刻删除整个旧平台 bus”不能同时无条件成立。
建议分清：

- 本阶段目标：普通平台发现/注册入口与 VirtIO-MMIO 链路统一进入新模型。
- 过渡例外：PCI 自己拥有的旧 host/bridge/endpoint 子树整体保留，不能同一
  host 又在新模型注册一个独立管理者，也不能默默接受拓扑丢失。
- 旧 platform bus 的最终删除取决于 PCI host 交接方案；须单独列出条件，
  不能在普通平台设备切换后提前宣称所有旧平台对象已退役。

这是待确认的范围边界，不是在本轮引入跨模型父引用或共享生命周期协议。

## 5. 首个验收配置与分阶段交付建议

配置详细证据见 [配置与 QEMU 清单](platform-migration-config-inventory.md)。
区分 defconfig 显式值、Kconfig 默认值、构建 feature 和 QEMU 实际挂载。

源码核对后的首选建议是 **AArch64 QEMU + modern-MMIO block/RNG + 平台串口
接管**，保留正常文件系统与独立 coverage 磁盘，关闭自动 net/vsock、不开图形。
runner 的 transport 选择作用于所有自动附加 VirtIO 设备，不是只作用于 RNG。
因此 block 是首个可启动验收配置的前置迁移项；不优先增加混合 PCI 存储 fixture
来绕过它。受控模块验收可以早于 block 迁移完成，但不能当成完整启动验收。
未迁移其他家族依然不计为完成。根盘不能作为解绑实验对象；block 退出专项
需要独立非根磁盘和明确的消费者/flush/mount 生命周期设计。

建议保留当前 AArch64 QEMU 默认配置作为兼容对照；新路径使用独立验收配置，
不改变默认行为。配置内普通平台 / MMIO 的新旧所有权在注册前固定，不能新
probe 失败后回落旧路径，不能两侧独立扫描同一设备再靠去重争抢。

验收需要分两个阶段，避免声称 RNG-only 足以切换启动配置：

1. **模块接线阶段**：transport / VirtIO bus / RNG 的模型、受控后端及资源
   所有权测试。可验证未绑定 child，不宣称完整系统或 coverage 磁盘链已切换。
2. **可启动配置阶段**：补齐实际 console 交接、存储/根文件系统及测试输出
   依赖。新 MMIO 路径若承载磁盘，就必须先接入 block；若使用保留的 PCI
   磁盘，须明确列为独立旧 PCI 子树，并验证实际 QEMU 参数和端点归属。
   不能为了省一个 block 迁移而偷偷继续按功能类型分流 MMIO。

无驱动的有效设备保持未绑定是目标语义，但不允许把原配置依赖的磁盘/网络
丢失包装成“正常未绑定”。未启用的网络、图形、vsock、9p 不计入首个最小配置
功能通过；默认配置切换则需按其实际启用功能另列 gate。

## 6. 待评审的职责图（非 API 定稿）

```text
固件 / 板级发现：符合平台人口规则的稳定设备事实
  → 启动装配：本次启动唯一入口选择、基础设施/PCI 归属排除
      → 旧注册入口：仅仍由旧模型负责的配置/子树
      → 新平台注册入口：描述、资源事实、拓扑 → 新 domain
          → 普通平台驱动
          → VirtIO-MMIO transport 驱动
              → 创建并拥有 VirtIO 下游设备
                  → RNG / block / net 等功能驱动
```

新模型不读取具体硬件来选择框架；transport 在自己的 probe 中识别版本/设备类型，
无功能的空 slot 不产生 VirtIO 功能 child。transport 移除必须先停止产生 child，
完成自己拥有的 child 退出后再释放服务；仅设置 parent 引用不会自动提供该协议。
本地 Linux 可核对 `drivers/virtio/virtio_mmio.c:572` 和 `:653`；平台 population
规则参考 `drivers/of/platform.c:443`。本文不把 Linux 的现成 API 名当作本仓实现。

## 7. 进入详细设计前需要确认

1. 接受保留旧 PCI 子树（包括其 platform host）作为明确例外，普通平台入口
   迁移不扩成 PCI 生命周期迁移吗？建议接受，但例外必须有退出条件。
2. 接受首个可启动验收配置包含 MMIO block/RNG 和平台串口接管，将 block
   迁移列为其依赖吗？模块测试与完整配置验收分开，不预设新组合已经能够启动。
3. 新平台人口规则需明确根/总线递归、available、资源表达限制、特殊已拥有节点
   与空 transport 的处理；不能用取消旧 match filter 代替这个设计。

确认后再形成需求验收表、对象/接口设计与 PR 依赖图，最后落 tickets。
当前没有新增开发票或启动实现，也不回写已经批准的 R1 设计范围。
