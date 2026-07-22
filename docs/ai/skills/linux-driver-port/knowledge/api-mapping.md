# Linux → x-kernel API 映射表

本表是 Phase 2（映射）的查表依据。**每一条 x-kernel 接口都来自项目源码**，括注源码位置。
查表规则：

- 命中 → 直接按"x-kernel 对应"列替换，记入驱动画像的"已映射"清单。
- 未命中（本表没有、源码也搜不到）→ 记入缺口，转 [`gaps.md`](gaps.md)，按 [`templates/gap-report.md`](../templates/gap-report.md) 上报。**不要自行编造一个相近接口。**

符号约定：`dev` = `&DeviceObject` 或 `Arc<DeviceObject>`；`DriverResult<T> = Result<T, DriverError>`；`ResResult<T> = Result<T, ResError>`。

---

## 1. 驱动模型与注册

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `struct platform_driver` + `.probe`/`.remove` | `impl DeviceDriver for XxxDriver`（`probe_device`/`remove`） | [kdevice/src/driver/mod.rs](../../../../../drivers/kdevice/src/driver/mod.rs)；样例 [driver_registry/block/sdmmc.rs](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs) |
| `static const struct of_device_id xxx_match[]` / `of_match_table` | `const XXX: FirmwareMatchSpec { alias, dt_compatibles, acpi_ids }` | [firmware_specs.rs](../../../../../drivers/kdriver/src/driver_registry/firmware_specs.rs) |
| `MODULE_DEVICE_TABLE(of, ...)` | 同上，`dt_compatibles` 数组即匹配表 | |
| `.driver.name` | `fn name(&self) -> &'static str` | |
| `module_platform_driver(xxx)` / `module_init` | `pub(super) fn descriptor() -> BoxedDriver { Arc::new(XxxDriver) }` + 编译期 `DRIVER_FACTORIES` | 无运行时 module；见 [`driver-anatomy.md`](driver-anatomy.md) |
| `platform_set_drvdata` / `platform_get_drvdata` | 无直接等价：把私有数据用 `Arc<T>` 持有，经 `kclass::publish_*` 发布 | Linux 的 `drvdata` 模式 → 用 RAII 所有权替代 |
| `module_pci_driver` | 同上，但 `bus_types()` 返回 `&[BusTypeId::PCI]` | [bus_type.rs](../../../../../drivers/kdevice/src/bus/bus_type.rs) |
| `MODULE_AUTHOR/LICENSE/DESCRIPTION` | 文件头 SPDX 许可注释 | `// SPDX-License-Identifier: Apache-2.0` |

**`DeviceDriver` trait 全貌**（`probe_device` 之外都有默认实现）：

```rust
pub trait DeviceDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn device_kind(&self) -> DeviceKind;            // Block/Char/Net/Display/Input/Vsock/Fs9p/Bus
    fn bus_types(&self) -> &'static [BusTypeId];    // PLATFORM / PCI
    fn matcher(&self) -> &dyn DeviceMatcher;        // 返回你的 FirmwareMatchSpec
    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()>;
    fn remove(&self, device: Arc<DeviceObject>) -> DriverResult<()> { Ok(()) }
    fn suspend(&self, device: Arc<DeviceObject>) -> DriverResult<()> { Ok(()) }
    fn resume(&self, device: Arc<DeviceObject>) -> DriverResult<()> { Ok(()) }
    fn shutdown(&self, device: Arc<DeviceObject>) {}
}
```

---

## 2. 设备资源（MMIO / IRQ / DMA）

### 2.1 MMIO 寄存器

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `platform_get_resource(pdev, IORESOURCE_MEM, n)` | `device.first_mmio()`（首个区域）或读 `device.resources()` | [object.rs:132](../../../../../drivers/kdevice/src/device/object.rs#L132)；多区域时遍历 `resources()` |
| `devm_platform_ioremap_resource` / `devm_ioremap` | 胶水层：`crate::iomap_first_mmio(dev, "name")?` 返回 `(NonNull<u8>, size)`；底层 `device_res::devm_iomap(dev, region, "name")` | [kdriver/src/lib.rs:93](../../../../../drivers/kdriver/src/lib.rs#L93)；[device-res/src/mmio.rs](../../../../../drivers/device-res/src/mmio.rs) |
| `ioremap`（非 devm） | `Io::map(MmioRegion{base,size}, "name")?`（RAII，drop 时 unmap） | |
| `readl/writel`（32-bit）、`readb/readw/readq`、`writeb…` | `Io::read32(offset)` / `write32(offset, val)`，另有 `read8/16/64`、`write8/16/64` | 带边界检查 + Acquire/Release fence；[mmio.rs](../../../../../drivers/device-res/src/mmio.rs) |
| `ioread32/iowrite32` | 同上（`Io` 句柄方法） | |
| `devm_platform_get_and_ioremap_resource` | `first_mmio()` + `devm_iomap`（或直接 `iomap_first_mmio`） | |
| `memcpy_fromio / memcpy_toio` | 用 `Io::as_ptr()->NonNull<u8>` 自行做 volatile 拷贝（无封装 helper） | |

> **两种 probe 取址模式（都在用，按硬件层约定选）：**
> - **映射后 vaddr 交给硬件层**（sdmmc 模式）：`let (vaddr, _) = iomap_first_mmio(dev, "x")?; unsafe { XxxDriver::new(vaddr.as_ptr() as usize) }`。
> - **物理基址交给硬件层，硬件层自管映射**（fxmac 模式）：`let mmio = first_mmio_resource(dev)?; XxxNic::init(mmio.base)`。
> 新驱动优先用前者（vaddr + `Io` 句柄），更安全。

### 2.2 中断（IRQ）

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `platform_get_irq(pdev, n)` / `of_irq_get` | `device.first_irq()` → `Option<IrqResource>`（已解析） | [object.rs:140](../../../../../drivers/kdevice/src/device/object.rs#L140) |
| `devm_request_irq(dev, irq, handler, flags, name, data)` | `device_res::devm_request_irq(dev, irq: IrqResource, Arc<dyn IrqHandler>)?` 或胶水 helper | [irq.rs](../../../../../drivers/device-res/src/irq.rs) |
| `request_irq`（非 devm） | `Irq::request(irq, Arc<dyn IrqHandler>)?`（RAII，drop 时释放） | |
| `irqreturn_t (*handler)(int irq, void *data)` | `impl IrqHandler { fn handle(&self) -> IrqEvent }`；**闭包 `Fn()->IrqEvent + Send+Sync` 自动实现** | 可直接 `Arc::new(|| { … IrqEvent::HANDLED })` |
| `IRQ_HANDLED` / `IRQ_NONE` | `IrqEvent::HANDLED` / `IrqEvent::NOT_HANDLED`（另有 `from_sources(u8)` 带事件位图） | |
| `IRQF_TRIGGER_*` | `IrqTrigger::{EdgeRising, EdgeFalling, LevelHigh, LevelLow, Unknown(u32)}` | DT 已解析时直接用 `first_irq()` 的 trigger |
| `disable_irq/enable_irq` | `Irq::set_enabled(bool)` | |
| `free_irq` | drop `Irq` 句柄（RAII） | |
| `request_threaded_irq` / 中断下半部 | **缺口**：无线程化 IRQ；见 [`gaps.md`](gaps.md)（workqueue/tasklet 缺失） | 上半部用 `IrqHandler`；延后处理见 gaps |

**中断处理闭包示例**（设备状态自引用时用 `Arc<Self>`）：

```rust
struct RxIrq { dev: Arc<MyDev> }
impl IrqHandler for RxIrq {
    fn handle(&self) -> IrqEvent {
        // 读中断状态寄存器 → 清中断 → 处理
        IrqEvent::HANDLED
    }
}
```

### 2.3 DMA

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `dmam_alloc_coherent` / `dma_alloc_coherent` | `device_res::devm_alloc_coherent(dev, DmaSpec)?` 返回 `(NonNull<u8>, bus_addr: u64)` | [dma.rs](../../../../../drivers/device-res/src/dma.rs) |
| `dma_alloc_coherent`（非 devm） | `DmaCoherent::alloc(DmaSpec)?`（RAII）；类型化用 `CoherentBox<T>::alloc(extra_align)` / `CoherentArray<T>::alloc(count, extra_align)` | |
| `DMA_TO_DEVICE/BIDIRECTIONAL/FROM_DEVICE` | `DmaDirection::{DriverToDevice, Bidirectional, DeviceToDriver}` | `DmaSpec::new(len, align)` 默认 Bidirectional；`.with_direction(d)` 覆盖 |
| `dma_free_coherent` | drop `DmaCoherent`（RAII） | |
| `dma_map_single / dma_unmap_single`（streaming） | `DmaOp::map_streaming`（默认 `Unsupported`，看宿主是否实现） | 多数路径走 coherent；streaming 视宿主 |
| `dma_sync_single_for_cpu/device` | `DmaOp::sync_for_cpu/sync_for_device`（默认 no-op） | coherent 内存无需 sync |
| `dma_set_mask_and_coherent` | （当前无显式掩码 API；RK3588 aarch64 默认 64-bit） | 视为可选，缺口记录 |

---

## 3. 设备类发布（把驱动实例挂进系统）

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `register_netdev` / `net_device_ops` | `impl NetDevice for …` + `kclass::publish_net(device, Box::new(dev))?` | [kclass/src/lib.rs:478](../../../../../drivers/kclass/src/lib.rs#L478)（宏生成） |
| `block_device_operations` + `add_disk` / `gendisk` | `impl BlockDevice for …` + `kclass::publish_block(device, Box::new(dev))?` | trait [block/src/lib.rs](../../../../../drivers/block/src/lib.rs)；样例 [block/sdmmc.rs](../../../../../drivers/block/src/sdmmc.rs) |
| `misc_register` / `cdev_init+add` / `tty_register_driver` | `impl CharDevice for …` + `impl Device` + `kclass::publish_char(device, Box::new(dev))?` | 样例 [char/serial.rs](../../../../../drivers/kdriver/src/driver_registry/char/serial.rs) |
| `platform_device_register`（静态设备） | 平台后端的 static device 机制（一般无需驱动侧操作） | |
| i2c/spi/watchdog/rtc/… subsystem 注册 | **缺口**：无对应 `DeviceKind` 与 class；见 [`gaps.md`](gaps.md) | 这是 RK3588 移植最常见的阻塞点 |

**`BlockDevice` trait**（`drivers/block/src/lib.rs`，继承 `Device`）：

```rust
pub trait BlockDevice: Device {
    fn num_blocks(&self) -> u64;
    fn block_size(&self) -> usize;
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult;
    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult;
    fn flush(&self) -> DriverResult;
}
```

**`Device` trait**（`driver_base`，所有设备基类）：

```rust
pub trait Device: Send + Sync {
    fn name(&self) -> &str;
    fn device_kind(&self) -> DeviceKind;
    fn irq(&self) -> Option<usize> { None }
}
```

---

## 4. 同步与并发

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `spinlock_t` + `spin_lock_irqsave/restore` | `kspin::SpinNoIrq<T>`（`SpinNoIrq::new(t)`，`.lock()` 取 guard） | [kspin](../../../../../task/kspin/)；最常用 |
| `spin_lock`（不关中断，仅防抢占） | `kspin::SpinNoPreempt<T>` | |
| `raw_spinlock_t` | `kspin::SpinRaw<T>` | |
| `rwlock_t` | `kspin::SpinRwNoIrq<T>` | |
| `DEFINE_MUTEX` / `mutex_lock/unlock` | `ksync::Mutex<T>`（`.lock()`） | 可睡眠上下文 |
| `rwlock`（可睡眠） | `ksync::RwLock<T>`（`.read()/.write()`） | |
| `struct semaphore` / `down/up` | `ksync::Semaphore::new(n)`（`.acquire()/.try_acquire()`） | |
| `atomic_t` / `atomic_set/atomic_read/atomic_inc` | `core::sync::atomic::{AtomicU32, AtomicUsize, …}` + `Ordering` | |
| `wait_event*/wait_queue_head_t` | `ktask::WaitQueue`（`.wait()`/`.wait_until(closure)`/`.wait_timeout(dur)`/`.notify_one(true)`/`.notify_all(true)`） | [wait_queue.rs](../../../../../task/ktask/src/wait_queue.rs) |
| `struct completion` / `wait_for_completion` | 无直接等价 → 用 `WaitQueue` 替代（缺口记录） | 见 [`gaps.md`](gaps.md) |
| `workqueue` / `INIT_WORK` / `schedule_work` | **缺口**：无 workqueue/tasklet；见 [`gaps.md`](gaps.md) | 延后工作需自建内核线程或上报缺口 |
| `RCU` | **缺口**：无 RCU；用 `Arc`/`RwLock` 替代并记录 | |

---

## 5. 内存分配

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `kmalloc/kzalloc` + `kfree` | `Box::new(x)` / `Vec::new()` / `alloc::vec![]`；`kfree` 由 drop 完成 | `alloc` crate |
| `devm_kmalloc/devm_kzalloc` | `Box::new(x)`，生命周期挂在设备上时用 `device.register_cleanup(Box::new(|| …))` 释放 | 无专用 devm 分配器；用 RAII + cleanup |
| `kcalloc` / 数组分配 | `vec![T::default(); n]` 或 `Vec::with_capacity(n)` | |
| `kmem_cache_create`（slab） | **缺口**：无 slab cache；用 `Box`/`Vec`，性能敏感场景上报 | |
| `get_zeroed_page / __get_free_page` | `axalloc::global_allocator().alloc_pages(pages, PAGE_SIZE_4K, UsageKind::Dma)`（fxmac 用法） | [fxmac.rs:27](../../../../../drivers/kdriver/src/driver_registry/net/fxmac.rs#L27) |
| `alloc_pages` / `__get_free_pages` | 同上 | |
| `vmalloc` | **缺口**：无 vmalloc 等价；大缓冲用 DMA coherent 或上报 | |
| `kmap/kunmap` | 无（aarch64 直接用虚拟地址） | |
| `copy_to_user/copy_from_user` | 无用户态拷贝 API（驱动侧一般用 `read/write` trait 的 `&[u8]`/`&mut [u8]`） | char 设备在 `CharDevice::read/write` 里直接操作 buf |

---

## 6. 时间、延迟与定时

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `msleep(ms)` / `schedule_timeout` | `ktask::sleep(core::time::Duration::from_millis(ms))` | [api.rs:336](../../../../../task/ktask/src/api.rs#L336) |
| `msleep_interruptible` | `ktask::sleep` + `WaitQueue` 条件 | |
| `usleep_range` | `ktask::sleep(Duration::from_micros(..))` | |
| `udelay / mdelay`（忙等） | **缺口**：无 busy-wait helper；可 `core::hint::spin_loop()` 自旋计数，缺口记录 | 见 [`gaps.md`](gaps.md) |
| `jiffies` / `msecs_to_jiffies` / `time_after` | `khal::time::monotonic_time_nanos()` / `TimeValue` 自行换算 | |
| `ktime_get` / `ktime_get_ns` | `khal::time::monotonic_time_nanos()` | |
| `setup_timer` / `timer_setup` / `mod_timer` | **缺口**：无通用软定时器；`ktask` 有 `register_timer_callback`（很薄）；hrtimer 缺 | 见 [`gaps.md`](gaps.md) |
| `jiffies_to_msecs` 等 | `Duration` 算术 | |

---

## 7. 日志与错误处理

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `dev_info/dev_err/dev_warn/dev_dbg` | `log::info!/error!/warn!/debug!`（无 `dev_` 变体，自行把 `device.location()` 或 name 拼进消息） | `log` crate |
| `printk / pr_info/pr_err` | `log::info!` 等 | |
| `-ENOMEM` | `DriverError::NoMemory` | [driver_base/src/lib.rs:137](../../../../../drivers/driver_base/src/lib.rs#L137) |
| `-EINVAL` | `DriverError::InvalidInput` | |
| `-ENODEV/-ENXIO` | `DriverError::BadState` 或 `InvalidInput`（视语义） | |
| `-EBUSY` | `DriverError::ResourceBusy` | |
| `-EIO` | `DriverError::Io` | |
| `-ENOTSUPP/-EOPNOTSUPP` | `DriverError::Unsupported` | |
| `-EEXIST` | `DriverError::AlreadyExists` | |
| 错误传播 `< 0` 返回 + `IS_ERR` | `?` 运算符 + `DriverResult<T>` / `ResResult<T>` | 用 `map_err` 在两套错误间转换 |

**`DriverError` 全集**：`AlreadyExists / WouldBlock / BadState / InvalidInput / Io / NoMemory / ResourceBusy / Unsupported`。
**`ResError` 全集**（device-res）：`InvalidResource / MappingFailed / NoMemory / Busy / Unsupported / NoProvider`。

---

## 8. 设备树属性读取

设备资源（reg/interrupts）一般已被总线层解析进 `DeviceObject`（用 `first_mmio()/first_irq()` 即可）。需要**直接读 DT 属性**时：

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `of_property_read_u32(node, "x", &val)` | `of::property_u32(node, "x") -> Option<u32>` | [of/src/lib.rs:89](../../../../../drivers/of/src/lib.rs#L89) |
| `of_find_compatible_node` | `of::find_compatible("vendor,x") -> Option<FdtNode>` | |
| `of_find_node_by_name/path` | `of::find_node("/path")` / `resolve_node(path_or_alias)` | |
| `of_get_named_gpio` | **缺口**：无 gpio；见 [`gaps.md`](gaps.md) | |
| `of_irq_by_resource / of_irq_get` | `device.first_irq()`（已解析）或 `of::first_interrupt_desc(node)` | |
| `of_address_to_resource` | `device.first_mmio()`（已解析） | |
| `of_property_read_string` | 经 `FdtNode` property 接口（`of::fdt()` 取底层 `LinuxFdt`） | 较薄，按需用 |
| `of_match_node` | 由总线层用 `FirmwareMatchSpec.dt_compatibles` 自动匹配 | 驱动侧只声明 spec |

---

## 9. 地址转换（DMA / 物理-虚拟）

设备做 DMA 时常需 vaddr↔paddr 转换（Linux 里 `virt_to_phys` / `dma_map_single` 隐含）：

| Linux | x-kernel 对应 | 备注 |
|-------|---------------|-------------|
| `virt_to_phys` | `khal::mem::v2p(VirtAddr) -> PhysAddr` | [fxmac.rs:18](../../../../../drivers/kdriver/src/driver_registry/net/fxmac.rs#L18) |
| `phys_to_virt` | `khal::mem::p2v(PhysAddr) -> VirtAddr` | |
| `PAGE_SIZE` | `khal::mem::PAGE_SIZE_4K` | |
| 物理地址类型 | `khal::mem::PhysAddr::from_usize(usize)` / `VirtAddr::from_usize(usize)` | serial.rs 用法 |

> **HAL 抽象模式**：当硬件逻辑层 crate（如 `drivers/net/src/fxmac`）定义了自己的 `KernelFunc` trait 需要 v2p/p2v/dma_alloc/irq 时，胶水层用 `#[crate_interface::impl_interface]` 提供宿主实现（见 [fxmac.rs:14-43](../../../../../drivers/kdriver/src/driver_registry/net/fxmac.rs#L14-L43)）。新驱动优先直接用 `device-res` 的 `devm_*`，避免再造 HAL trait；仅当复用既有硬件 crate 时才走 HAL 桥接。

---

## 10. 缺失子系统速查（详见 `gaps.md`）

以下 Linux 子系统在 x-kernel **当前无对应实现**，转写时几乎必撞，**一律按缺口上报**，不得静默 stub：

`clk framework` · `regmap` · `gpio (gpiod)` · `pinctrl` · `reset controller` · `regulator` · `iommu` · `phy` · `workqueue/tasklet` · `completion`（用 `WaitQueue` 替代并记录）· `udelay/mdelay` · `hrtimer/timerfd` · `i2c/spi/watchdog/rtc/mmc subsystem`（无对应 `DeviceKind`/class）· `vmalloc` · `RCU` · `kmem_cache`。

替代方案与报告口径见 [`gaps.md`](gaps.md)。
