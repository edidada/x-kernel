# drv-block — 安全与可靠性分析

## Scope

`drivers/drv/block/src/{lib,common,ramdisk}.rs`（138 行）。

## 信任模型

本 crate 是**受信内核代码**，不接受外部输入。它的两个输入源都是编译期确定的：

| 输入 | 来源 | 信任级别 |
|---|---|---|
| `DeviceDescriptor`（alias `"ramdisk"`、`PlatformStatic{id:0}`） | `driver-compose::enumerate_static_devices()` 的硬编码常量 | 可信 |
| `KFEAT_RAMDISK_SIZE` | Kconfig → `kbuild_config` | 可信（构建参数） |
| 内嵌镜像（`ramdisk-static`） | 构建期 `include_bytes!` | 可信 |

没有固件输入、没有设备输入、没有用户输入。

## 外部边界 / 攻击面

| 边界 | 是否触及 |
|---|---|
| 用户内存 / 用户指针 | 否 |
| MMIO / PIO 寄存器 | **否**——ramdisk 是纯内存设备 |
| DMA 缓冲 / 设备可写内存 | 否 |
| 中断 | 否 |
| 固件 / 设备树 / ACPI | 否（板级静态设备，无固件描述） |
| 文件系统 / 网络 / IPC 数据 | **间接**：块设备的**读出内容**会被上层当文件系统镜像解析。这不是本 crate 的攻击面（内容由 `block::ramdisk_image` 或用户写入的堆盘决定），但它是 `ramdisk-static` 的信任假设：内嵌镜像被视为可信的根文件系统 |

因此本 crate 的实际攻击面是**空的**；它出现在这份文档里是因为它是块类驱动的
模板，模板的安全形状会被复制。

## unsafe 代码清单

无。

## 内存安全不变量

1. **`RamdiskData` 的发布 guard 是第一个字段**，因此 drop 时先撤销发布再释放
   其他资源。当前它是唯一字段，但顺序约定必须保留——新增字段（例如未来的
   IRQ guard）时要放在它**之后**。
2. **`Gendisk` 的强引用只在 class registry 里**。`RamdiskData` 不持有，因此
   "撤销发布"就是"释放磁盘"，不存在两条释放路径。
3. **`major=1, first_minor=0, minors=1`** 是硬编码的单一设备号。两个 ramdisk
   实例会撞号——由 `publish_block` 的查重（`AlreadyExists`）与 devnode 的
   index 池挡住，不是靠本 crate。

## 线程安全

本 crate 无状态：`RamdiskDriver` 是零大小类型，`RamdiskData` 只含一个
`ClassWithdraw`（内部是 `Arc` + 原子标志）。`Send + Sync` 由字段类型继承。

块 I/O 的并发在 `subsystems/block`（`SpinNoIrq` 保护的请求队列 + softirq
reclaimer），不在这里。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|---|---|---|---|---|
| T-01 | `ramdisk-static` 的内嵌镜像被替换成恶意文件系统镜像，内核挂载它作为根 | 高 | 构建链被污染 | **不在本 crate 防御**：镜像是构建期 `include_bytes!` 的产物，信任边界在构建系统。本 crate 只是把它交给 `block::ramdisk_image`。**残余风险**：与任何内嵌 initramfs 相同 |
| T-02 | `KFEAT_RAMDISK_SIZE` 设得过大，`RamDisk::new` 分配失败或吃光内存 | 中 | 配置错误 | **部分实现**：分配失败会让 `probe` 返回 `Fatal` → 设备 `Unclaimed`，不会 panic。**残余风险**：一个"成功但吃掉大半内存"的配置没有上限检查。加固建议：在 Kconfig 的 help 里写明上限，或在 `RamDisk::new` 里对容量做上界检查 |
| T-03 | 堆盘（非 static 变体）的内容被用户态写入后，内核把它当文件系统解析 | 中 | 用户态打开 `/dev/ramdisk` 写入 | **不在本 crate 防御**：这是块设备的正常语义，解析责任在文件系统层（`fs/filesystems/*`），它们必须把磁盘内容当**不可信输入**。本 crate 只提供一个零填充的字节数组 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|---|---|---|---|---|---|---|
| F-01 | `Gendisk::new` 失败 | 名字为空 / `minors == 0` / minor 范围溢出 / 容量校验失败（`block_size == 0` 或 `num_blocks * block_size` 溢出） | `ProbeError::Fatal(Invalid)` | 设备 `Unclaimed`；若它是根文件系统来源则启动失败 | 2 | `Gendisk::new` 在 `subsystems/block` 里做全部四项校验；本 crate 只 `map_err` |
| F-02 | `publish_block` 返回 `AlreadyExists` | 同一 owner id 重复发布 | probe 失败 | 已发布的磁盘不受影响 | 3 | `device-class` 的注册表查重 |
| F-03 | devnode 投影失败（index 耗尽 / 重复 id） | `devnode::publish*` 返回错误 | `/dev/ramdisk` 缺失 | 磁盘已发布但用户态打不开；uevent 不会触发 udev 规则 | 3 | **本 crate 不感知**：`with_devnode_context` 只设置上下文，投影失败在 `device-class`/`devnode` 层记录（见 `devnode/docs/security.md` F-04，订阅者失败无错误通道） |
| F-04 | `RamDisk::new` 内存分配失败 | `KFEAT_RAMDISK_SIZE` 过大或系统内存紧张 | probe 失败 | 同 F-01 | 2 | 分配走 `Box`/`Vec`，失败是 `alloc` 错误不是 panic |

## 故障管理

- `probe` 的所有失败都经 `crate::common::map_driver_error` 变成
  `ProbeError::Fatal(RuntimeError)`——**ramdisk 没有可 defer 的依赖**（不消费
  时钟、不需要 IRQ、不需要其他 provider），所以永远不会返回 `Deferred`。
  这是模板价值的一部分：它展示了"无依赖驱动"的最小形状。
- `remove` 不返回 `Result`（`DriverOps::remove` 签名如此），也不需要：没有硬件
  要 quiesce。
- 本 crate 不 panic。

## 隐私分析

无。ramdisk 的内容是零填充或构建期内嵌镜像；本 crate 不读写用户数据。
堆盘变体的内容由用户态写入，但那是块设备的正常语义，本 crate 不做任何解析。

## 已知限制

1. **只支持一个 ramdisk 实例**（硬编码 `major=1, first_minor=0, minors=1`）。
   Linux brd 支持 `rd_nr` 个实例。
2. **不支持分区**（`minors=1`，只有 part0）。
3. **堆盘变体是零填充的，仅用于 bring-up**——没有持久化，重启即丢。
   Kconfig 的 help 已写明。
4. **`remove` 是空的**，因此重新 probe 会拿到一个全新的零盘（堆变体）；
   static 变体的镜像是只读的，重新 probe 恢复初始内容。这是正确行为，
   但与"移除后数据保留"的直觉不同，值得记录。

## 审计清单

- [ ] 新增字段时是否放在 `_publication` **之后**？（不变量 1）
- [ ] 新驱动是否真的没有可 defer 的依赖？如果有（时钟/IRQ domain/其他设备），
      必须返回 `ProbeError::Deferred` 而不是 `Fatal`——用 `Fatal` 会让设备过早
      `Unclaimed`。
- [ ] 是否调用了 `with_devnode_context`？漏掉它磁盘不会到达 `/dev`（F-03）。
- [ ] `ClassParent::detached` 的 owner 是否用 `dev.id().raw()`？用别的 id 会让
      移除时找不到这个发布（`device-class` 按 owner 匹配）。
- [ ] 有没有在这里加块类的通用逻辑？那属于 `subsystems/block`，不属于驱动 crate。
