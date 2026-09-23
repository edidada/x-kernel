# Linux → x-kernel 驱动转写映射表（driver-core 模型）

> 规范性文档（normative）：转写 Linux 驱动到 x-kernel `driver-core` 模型时，
> 按本表做机械映射。Golden 模板见 `drivers/drv/`：
> `serial-pl011`（platform/DT 消费者）、`fixed-clock`（provider）、
> `virtio-blk-core`（PCI 消费者 + 类发布 + devnode）、
> `virtio-net-core`（中断所有权）、`virtio-rng-core`（最小消费者）、
> `ramdisk`（板级静态设备）。

## 0. 转写流程（一个驱动的完整步骤）

1. **建 crate**：`drivers/drv/<name>`，单文件 `src/lib.rs`；
2. **翻译 id 表**：`*_device_id` 数组 → `static IDS: &[<Bus>::IdEntry]`；
3. **写 DriverOps**：`probe`/`remove`（`remove` 通常为空——`Data` 的 Drop 负责释放）；
6. **注册**：`pub fn register()` → 组合根 `driver-compose` 一行调用。
4. 资源获取一律走 `DeviceCtx`，**全部 guard 存进 `Self::Data`**（字段声明顺序 = 释放顺序）；
5. 类发布用 `kclass::publish_*(ClassParent::detached(...), ...)`。

## 1. 驱动骨架

| Linux | x-kernel |
|---|---|
| `struct platform_driver` | `struct FooDriver;` + `impl DriverOps<PlatformBus>` |
| `struct pci_driver` | `impl DriverOps<PciBus>` |
| `probe(dev)` | `fn probe(&self, dev: &mut DeviceCtx<'_, Bus>) -> Result<Self::Data, ProbeError>` |
| `remove(dev)` | `fn remove(&self, dev: &mut BoundDevice<'_, Bus, Self::Data>)` |
| `struct foo_priv`（dev_get_drvdata） | `type Data = FooData`（框架所有，`remove` 可 `data_mut()`） |
| `module_platform_driver()` / `module_init()` | `pub fn register()`，由 `driver-compose` 的 feature 调用 |
| `MODULE_DEVICE_TABLE`（自动 modalias） | 领域从匹配的 id 行计算，经 `dev.modalias()` → uevent `MODALIAS=` |

## 2. id 表（数据，不是代码）

| Linux | x-kernel |
|---|---|
| `static const struct of_device_id foo_of_match[] = { { .compatible = "x,y" } }` | `static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("x,y"))]` |
| `{ .compatible = ... }`（板级别名） | `PlatformId::Alias(AliasMatch::new("serial0"))` |
| `static const struct pci_device_id foo_pci_tbl[] = { { PCI_DEVICE(0x8086, 0x100e) } }` | `PciMatch::device(0x8086, 0x100e)`（modern + transitional 各一行；**transitional 系列按类型索引：0x1000=net, 0x1001=blk, 0x1005=rng**——写错行会静默认领别人的设备） |
| `MODULE_DEVICE_TABLE` 的 modalias | `pci:v00001AF4d00001001` / `of:NcompatT(null)C<x,y>`（框架自动计算） |

## 3. 设备资源（DeviceCtx）

| Linux | x-kernel | 返回 |
|---|---|---|
| `platform_get_resource(pdev, IORESOURCE_MEM, n)` | `dev.mmio(n)` / `dev.mmio_by_name("regs")` | `MmioRegion`（值） |
| `platform_get_irq(pdev, n)` / `platform_get_irq_byname` | `dev.irq(n)` / `dev.irq_by_name("rx")` | `IrqSpec` |
| `devm_ioremap_resource(dev, res)` | `dev.iomap(n)` / `dev.iomap_named("regs")` | `IoMap` guard（**必须存进 Data**） |
| `devm_request_irq(dev, irq, handler, flags, name, dev_id)` | `dev.request_irq(spec, "name", move \|\| ...)`（闭包捕获 `Weak` 到自己的 Data 或内部状态，即 dev_id 模式） | `DevIrq` guard（**必须存进 Data，且声明为第一个字段**——中断先于映射释放） |
| `devm_kzalloc` | Data 结构体的普通字段 | — |
| `dma_alloc_coherent` / `dmam_alloc_coherent` | `dev.alloc_coherent(len, align)` | `CoherentMem` guard |
| `clk_get(dev, "apb_pclk")` | `dev.clk("apb_pclk")` | `ClkHandle`（provider 未注册时自动 `-EPROBE_DEFER`） |
| `clk_prepare_enable(clk)` / `clk_disable_unprepare` | `clk.prepare_enable()` / `clk.disable_unprepare()` | — |
| `of_clk_add_hw_provider()` | `driver_runtime::register_clk_provider(&'static dyn ClkLookup)`（probe 内调用，唤醒等待者） | — |
| `-EPROBE_DEFER` | `Err(ProbeError::Deferred(DepSource::...))`（一般不用手写——`dev.clk()` 等会自动） | — |
| MMIO 寄存器读写 `readl/writel` | `IoMap::read32/write32`（带边界检查和内存屏障） | — |

## 4. 中断与并发

| Linux | x-kernel |
|---|---|
| `request_irq` 的 `dev_id` 参数 | handler 闭包捕获设备内部状态的共享句柄（`Arc<SpinNoIrq<...>>`），**没有全局 IRQ 路由表** |
| 共享中断线遍历 | kirq 的 action 机制处理；handler 返回 `IrqAction::NotHandled` 表示非本设备 |
| `devm_request_threaded_irq` | `dev.request_irq`（worker 化在 kirq 层，接口一致） |

## 5. 类发布（用户态可见性）

| Linux | x-kernel |
|---|---|
| `device_create()` / `register_chrdev` | `kclass::publish_char(ClassParent::detached(name, location), Box::new(dev))`（char） |
| `add_disk()` | `Gendisk::new(...).with_devnode_context(parent_path, modalias)` → `kclass::publish_block(...)`；`add_disk` 自动投影 `/dev/<name>` + uevent |
| `device_add_disk` 的 uevent | devnode 注册表 → `kdevice_uevent` 发布 `ACTION/DEVPATH/SUBSYSTEM/MAJOR/MINOR/DEVNAME/MODALIAS` |
| 平台驱动注册 clock provider | `dev.clk()` 的对偶：probe 末尾 `register_clk_provider` |

## 6. 组合根（driver-compose/src/lib.rs）

```rust
pub fn init() {
    driver_runtime::XKernelRuntime::install_global();   // 服务装配
    // 移除通知：设备移除时清理 detached 类发布
    global_domain().set_remove_notifier(...);
    // 驱动注册（一行一个，feature 门控）
    #[cfg(feature = "fixed-clock")]  fixed_clock::register();
    #[cfg(feature = "serial-pl011")] serial_pl011::register();
    #[cfg(feature = "virtio-blk")]   { block::dispatch::reclaimer::init(); virtio_blk_core::register(); }
    // 枚举（固件 DT + 板级静态 + PCI）
    enumerate_firmware_devices();
    enumerate_static_devices();
    enumerate_pci_devices();
}
```

新驱动接入 = ①新 crate；②compose 加 feature + 一行注册；③Kconfig 符号
`KFEAT_DRIVER_CORE_<NAME>`；④kfeat 特性映射。**不改框架任何文件。**

## 7. Kconfig 接线（迁移开关模式）

```
config KFEAT_DRIVER_CORE_<NAME>
    bool "<NAME> on the driver-core model"
    default n
    help
      Migration target for KFEAT_DRIVER_<NAME>; enable it instead of
      (not in addition to) the legacy symbol.
```

`api/kfeat/Cargo.toml`：
```toml
driver_core_<name> = [
    "dep:driver-compose", "dep:<name>-core",
    "kdriver/driver-core-init",
    "driver-compose/<name>",
    # 类依赖（如 char/net/block）在此拉齐
]
```

注意：如果 QEMU 需要为该设备挂硬件（如 virtio-blk/net/rng），
`xtask/xkmake/src/qemu.rs` 的 `needs_*` 要同时认新旧符号。

## 8. 验证

- 单 crate 类型检查：`make check-crate CRATE=<pkg>`
- 内核单测：`make unittest UNITTEST_CRATE=<module_path>`（下划线 crate 名）
- 实机：`make build && make justrun`（新符号开、旧符号关，看 probe 日志和消费者）

## 9. 已知不映射项（长期项）

| Linux 概念 | 状态 |
|---|---|
| `struct page`/DMA scatter-gather API | `device_res` 的 streaming DMA（map_streaming/unmap_streaming）已覆盖单缓冲区；SG 待补 |
| `struct work_struct` / delayed work | 用 `ktask`/`kwork`（非驱动模型管辖） |
| `devm_*` 完整矩阵（devm_kmemdup 等） | guards-in-Data 覆盖主路径；特殊形态按需加 `DeviceCtx` 方法 |
| runtime PM（pm_runtime_get/put） | 未建模（`suspend/resume` 在 `DeviceDriver` 预留位，尚未接 PM 框架） |
| virtio transport 的 MMIO/DMA 映射所有权 | 仍由 vendored `virtio-drivers` HAL 持有（映射生命周期 = transport 生命周期）；内核自有 transport 是后续工作 |
| MSI-X per-queue向量 | `device_res::MsiResource` 已有分配接口，`DeviceCtx` 方法未暴露 |
