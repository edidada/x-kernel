<!-- 本文件由 `ldp gen-doc` 从 data/api-mapping.json 生成。勿手改——改 data/api-mapping.json 后重跑 `ldp gen-doc`。 -->

# Linux → x-kernel API 映射表

Phase 2(映射)的查表依据。**每一条 x-kernel 接口都来自项目源码**(括注源码位置)。查表规则:命中 → 按「x-kernel 对应」替换;未命中 → 记缺口,转 `gaps.md`。**不要自行编造相近接口(铁律 2)。**

**命令式查询**:跑 `ldp lookup <api>` 拿结构化候选(显式截断,不全文倾倒)。

符号约定:`dev` = `&DeviceObject`/`Arc<DeviceObject>`;`DriverResult<T> = Result<T, DriverError>`;`ResResult<T> = Result<T, ResError>`。

## 1. 驱动模型与注册

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `struct platform_driver + .probe/.remove` | impl DeviceDriver for XxxDriver (probe_device/remove) | DeviceDriver trait 的 probe_device/remove/suspend/resume/shutdown;除 probe_device 外都有默认实现 [drivers/kdevice/src/driver/mod.rs] |
| `static const struct of_device_id xxx_match[] / of_match_table` | const XXX: FirmwareMatchSpec { alias, dt_compatibles, acpi_ids } | dt_compatibles 即匹配表 [drivers/kdriver/src/driver_registry/firmware_specs.rs] |
| `module_platform_driver(xxx) / module_init` | pub(super) fn descriptor() -> BoxedDriver { Arc::new(XxxDriver) } + 编译期 DRIVER_FACTORIES | 无运行时 module [drivers/kdriver/src/driver_registry] |
| `platform_set_drvdata / platform_get_drvdata` | 用 Arc<T> 持有私有数据,经 kclass::publish_* 发布 | 无直接等价;drvdata 模式 → RAII 所有权替代 |
| `module_pci_driver` | 同 platform,但 bus_types() 返回 &[BusTypeId::PCI] | [drivers/kdevice/src/bus/bus_type.rs] |
| `MODULE_AUTHOR/LICENSE/DESCRIPTION` | 文件头 SPDX 许可注释 | // SPDX-License-Identifier: Apache-2.0 |

## 2. 设备资源 — MMIO 寄存器

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `platform_get_resource(pdev, IORESOURCE_MEM, n)` | device.first_mmio()(首个区域)或遍历 device.resources() | 多区域时遍历 resources() [drivers/kdevice/src/device/object.rs:132] |
| `devm_platform_ioremap_resource / devm_ioremap` | 胶水:crate::iomap_first_mmio(dev, "name")? 返回 (NonNull<u8>, size);底层 device_res::devm_iomap(dev, region, "name") | 推荐 vaddr 模式 [drivers/kdriver/src/lib.rs:93] |
| `ioremap(非 devm)` | Io::map(MmioRegion{base,size}, "name")?(RAII,drop 时 unmap) | [drivers/device-res/src/mmio.rs] |
| `readl/writel, readb/readw/readq, writeb…` | Io::read32(offset) / write32(offset, val);另有 read8/16/64、write8/16/64 | 带边界检查 + Acquire/Release fence [drivers/device-res/src/mmio.rs] |
| `ioread32/iowrite32` | 同上(Io 句柄方法) | [drivers/device-res/src/mmio.rs] |
| `devm_platform_get_and_ioremap_resource` | first_mmio() + devm_iomap(或直接 iomap_first_mmio) |  |
| `memcpy_fromio / memcpy_toio` | 用 Io::as_ptr()->NonNull<u8> 自行 volatile 拷贝(无封装 helper) | 无 helper |

## 3. 设备资源 — 中断(IRQ)

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `platform_get_irq(pdev, n) / of_irq_get` | device.first_irq() -> Option<IrqResource>(已解析) | [drivers/kdevice/src/device/object.rs:140] |
| `devm_request_irq(dev, irq, handler, flags, name, data)` | device_res::devm_request_irq(dev, irq: IrqResource, Arc<dyn IrqHandler>)? 或胶水 helper | [drivers/device-res/src/irq.rs] |
| `request_irq(非 devm)` | Irq::request(irq, Arc<dyn IrqHandler>)?(RAII,drop 释放) | [drivers/device-res/src/irq.rs] |
| `irqreturn_t (*handler)(int irq, void *data)` | impl IrqHandler { fn handle(&self) -> IrqEvent };闭包 Fn()->IrqEvent + Send+Sync 自动实现 | 可直接 Arc::new(|| { … IrqEvent::HANDLED }) [drivers/device-res/src/irq.rs] |
| `IRQ_HANDLED / IRQ_NONE` | IrqEvent::HANDLED / IrqEvent::NOT_HANDLED(另有 from_sources(u8)) | [drivers/device-res/src/irq.rs] |
| `IRQF_TRIGGER_*` | IrqTrigger::{EdgeRising, EdgeFalling, LevelHigh, LevelLow, Unknown(u32)} | DT 已解析时直接用 first_irq() 的 trigger [drivers/device-res/src/irq.rs] |
| `disable_irq/enable_irq` | Irq::set_enabled(bool) | [drivers/device-res/src/irq.rs] |
| `free_irq` | drop Irq 句柄(RAII) | [drivers/device-res/src/irq.rs] |
| `request_threaded_irq / 中断下半部` `missing` | 缺口:无线程化 IRQ;上半部用 IrqHandler,延后处理见 gaps(workqueue) | status=missing |

## 4. 设备资源 — DMA

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `dmam_alloc_coherent / dma_alloc_coherent` | device_res::devm_alloc_coherent(dev, DmaSpec)? 返回 (NonNull<u8>, bus_addr: u64) | [drivers/device-res/src/dma.rs] |
| `dma_alloc_coherent(非 devm)` | DmaCoherent::alloc(DmaSpec)?(RAII);类型化 CoherentBox<T>::alloc / CoherentArray<T>::alloc | [drivers/device-res/src/dma.rs] |
| `DMA_TO_DEVICE/BIDIRECTIONAL/FROM_DEVICE` | DmaDirection::{DriverToDevice, Bidirectional, DeviceToDriver} | DmaSpec::new 默认 Bidirectional;.with_direction(d) 覆盖 [drivers/device-res/src/dma.rs] |
| `dma_free_coherent` | drop DmaCoherent(RAII) | [drivers/device-res/src/dma.rs] |
| `dma_map_single / dma_unmap_single(streaming)` | DmaOp::map_streaming(默认 Unsupported,看宿主是否实现) | 多数路径走 coherent [drivers/device-res/src/dma.rs] |
| `dma_sync_single_for_cpu/device` | DmaOp::sync_for_cpu/sync_for_device(默认 no-op) | coherent 内存无需 sync [drivers/device-res/src/dma.rs] |
| `dma_set_mask_and_coherent` | 当前无显式掩码 API;RK3588 aarch64 默认 64-bit | 视为可选,缺口记录 |

## 5. 设备类发布(把驱动实例挂进系统)

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `register_netdev / net_device_ops` | impl NetDevice for … + kclass::publish_net(device, Box::new(dev))? | 宏生成 [drivers/kclass/src/lib.rs:478] |
| `block_device_operations + add_disk / gendisk` | impl BlockDevice for … + kclass::publish_block(device, Box::new(dev))? | [drivers/block/src/lib.rs] |
| `misc_register / cdev_init+add / tty_register_driver` | impl CharDevice for … + impl Device + kclass::publish_char(device, Box::new(dev))? | [drivers/kdriver/src/driver_registry/char/serial.rs] |
| `platform_device_register(静态设备)` | 平台后端的 static device 机制(一般无需驱动侧操作) |  |
| `i2c/spi/watchdog/rtc/… subsystem 注册` `missing` | 缺口:无对应 DeviceKind 与 class;见 gaps | RK3588 移植最常见的阻塞点;status=missing |

## 6. 同步与并发

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `spinlock_t + spin_lock_irqsave/restore` | kspin::SpinNoIrq<T>(SpinNoIrq::new(t),.lock() 取 guard) | 最常用 [task/kspin] |
| `spin_lock(不关中断,仅防抢占)` | kspin::SpinNoPreempt<T> | [task/kspin] |
| `raw_spinlock_t` | kspin::SpinRaw<T> | [task/kspin] |
| `rwlock_t` | kspin::SpinRwNoIrq<T> | [task/kspin] |
| `DEFINE_MUTEX / mutex_lock/unlock` | ksync::Mutex<T>(.lock()) | 可睡眠上下文 |
| `rwlock(可睡眠)` | ksync::RwLock<T>(.read()/.write()) |  |
| `struct semaphore / down/up` | ksync::Semaphore::new(n)(.acquire()/.try_acquire()) |  |
| `atomic_t / atomic_set/atomic_read/atomic_inc` | core::sync::atomic::{AtomicU32, AtomicUsize, …} + Ordering |  |
| `wait_event*/wait_queue_head_t` | ktask::WaitQueue(.wait()/.wait_until(closure)/.wait_timeout(dur)/.notify_one(true)/.notify_all(true)) | [task/ktask/src/wait_queue.rs] |
| `struct completion / wait_for_completion` `workaround` | 无直接等价 → 用 WaitQueue 替代(缺口记录) | 见 gaps;status=workaround [task/ktask/src/wait_queue.rs] |
| `workqueue / INIT_WORK / schedule_work` `missing` | 缺口:无 workqueue/tasklet;见 gaps | 延后工作需自建内核线程或上报;status=missing |
| `RCU` `workaround` | 缺口:无 RCU;用 Arc/RwLock 替代并记录 | status=workaround |

## 7. 内存分配

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `kmalloc/kzalloc + kfree` | Box::new(x) / Vec::new() / alloc::vec![];kfree 由 drop 完成 | alloc crate |
| `devm_kmalloc/devm_kzalloc` | Box::new(x),生命周期挂设备时用 device.register_cleanup(Box::new(|| …)) | 无专用 devm 分配器;RAII + cleanup |
| `kcalloc / 数组分配` | vec![T::default(); n] 或 Vec::with_capacity(n) |  |
| `kmem_cache_create(slab)` `missing` | 缺口:无 slab cache;用 Box/Vec,性能敏感上报 | status=optional |
| `get_zeroed_page / __get_free_page` | axalloc::global_allocator().alloc_pages(pages, PAGE_SIZE_4K, UsageKind::Dma) | fxmac 用法 [drivers/kdriver/src/driver_registry/net/fxmac.rs:27] |
| `alloc_pages / __get_free_pages` | 同 get_zeroed_page |  |
| `vmalloc` `workaround` | 缺口:无 vmalloc 等价;大缓冲用 DMA coherent 或上报 | status=workaround |
| `kmap/kunmap` | 无(aarch64 直接用虚拟地址) |  |
| `copy_to_user/copy_from_user` | 无用户态拷贝 API;char 设备在 CharDevice::read/write 里直接操作 &mut [u8]/&[u8] |  |

## 8. 时间、延迟与定时

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `msleep(ms) / schedule_timeout` | ktask::sleep(core::time::Duration::from_millis(ms)) | [task/ktask/src/api.rs:336] |
| `msleep_interruptible` | ktask::sleep + WaitQueue 条件 |  |
| `usleep_range` | ktask::sleep(Duration::from_micros(..)) |  |
| `udelay / mdelay(忙等)` `workaround` | 缺口:无 busy-wait helper;core::hint::spin_loop() 自旋 + monotonic_time_nanos 截止循环 | 见 gaps;status=workaround |
| `jiffies / msecs_to_jiffies / time_after` | khal::time::monotonic_time_nanos() / TimeValue 自行换算 |  |
| `ktime_get / ktime_get_ns` | khal::time::monotonic_time_nanos() |  |
| `setup_timer / timer_setup / mod_timer` `workaround` | 缺口:无通用软定时器;ktask 有 register_timer_callback(很薄);hrtimer 缺 | 见 gaps;status=workaround |

## 9. 日志与错误处理

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `dev_info/dev_err/dev_warn/dev_dbg` | log::info!/error!/warn!/debug!(无 dev_ 变体,自行拼 device.location()/name) | 早期 init 的 log::* 静默,改用 kernel_boot::bootln! |
| `printk / pr_info/pr_err` | log::info! 等 |  |
| `-ENOMEM` | DriverError::NoMemory | [drivers/driver_base/src/lib.rs:137] |
| `-EINVAL` | DriverError::InvalidInput | [drivers/driver_base/src/lib.rs:137] |
| `-ENODEV/-ENXIO` | DriverError::BadState 或 InvalidInput(视语义) | [drivers/driver_base/src/lib.rs:137] |
| `-EBUSY` | DriverError::ResourceBusy | [drivers/driver_base/src/lib.rs:137] |
| `-EIO` | DriverError::Io | [drivers/driver_base/src/lib.rs:137] |
| `-ENOTSUPP/-EOPNOTSUPP` | DriverError::Unsupported | [drivers/driver_base/src/lib.rs:137] |
| `-EEXIST` | DriverError::AlreadyExists | [drivers/driver_base/src/lib.rs:137] |
| `错误传播 < 0 返回 + IS_ERR` | ? 运算符 + DriverResult<T> / ResResult<T>;map_err 在两套错误间转换 |  |

## 10. 设备树属性读取

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `of_property_read_u32(node, "x", &val)` | of::property_u32(node, "x") -> Option<u32> | [drivers/of/src/lib.rs:89] |
| `of_find_compatible_node` | of::find_compatible("vendor,x") -> Option<FdtNode> | [drivers/of/src/lib.rs] |
| `of_find_node_by_name/path` | of::find_node("/path") / resolve_node(path_or_alias) | resolve_node 偶有不稳;稳健按结构找 fdt.all_nodes().find(...) [drivers/of/src/lib.rs] |
| `取设备时钟(clocks/clock-names)` | clk::devm_clk_get(device, "ciu") -> ResResult<Clk>(总线已预解析 ResourceDesc::Clock) | 驱动不碰 DT;详见 clk-subsystem [drivers/clk] |
| `of_get_named_gpio` `missing` | 缺口:无 gpio;见 gaps | status=missing |
| `of_irq_by_resource / of_irq_get` | device.first_irq()(已解析)或 of::first_interrupt_desc(node) |  |
| `of_address_to_resource` | device.first_mmio()(已解析) |  |
| `of_property_read_string` | 经 FdtNode property 接口(of::fdt() 取底层 LinuxFdt) | 较薄 |
| `of_match_node` | 由总线层用 FirmwareMatchSpec.dt_compatibles 自动匹配 | 驱动侧只声明 spec |

## 11. 地址转换(DMA / 物理-虚拟)

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `virt_to_phys` | khal::mem::v2p(VirtAddr) -> PhysAddr | [drivers/kdriver/src/driver_registry/net/fxmac.rs:18] |
| `phys_to_virt` | khal::mem::p2v(PhysAddr) -> VirtAddr |  |
| `PAGE_SIZE` | khal::mem::PAGE_SIZE_4K |  |
| `物理地址类型` | khal::mem::PhysAddr::from_usize / VirtAddr::from_usize | serial.rs 用法 |

## 缺失子系统速查(详见 `gaps.md`)

以下 Linux 子系统在 x-kernel 当前无对应实现,转写时几乎必撞,**一律按缺口上报**,不得静默 stub。
> 完整清单、阻塞程度、替代方案、报告口径见 `gaps.md`(同样由 `ldp gen-doc` 生成)。

---

## 参考(源码引用,gen-doc 原样保留)

以下为源码 trait 签名等**参考信息**,非结构化数据,由 gen-doc 的受保护附录块承载。
若要新增参考,编辑本文件 `GEN-DOC:APPENDIX` 标记之间的内容(不会被覆盖)。

<!-- GEN-DOC:APPENDIX-START (受保护参考块,gen-doc 原样保留) -->

**`DeviceDriver` trait 全貌**(`probe_device` 之外都有默认实现):

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

**两种 probe 取址模式**(按硬件层约定选):
- vaddr 模式(sdmmc,推荐):`iomap_first_mmio(dev,"name")` 拿 `(NonNull<u8>,size)`,`unsafe { XxxDriver::new(vaddr.as_ptr() as usize) }`。
- paddr 模式(fxmac):`first_mmio_resource(dev)?` 拿 `MmioRegion.base`(物理),硬件层自管映射。

**中断处理闭包示例**(设备状态自引用时用 `Arc<Self>`):

```rust
struct RxIrq { dev: Arc<MyDev> }
impl IrqHandler for RxIrq {
    fn handle(&self) -> IrqEvent {
        // 读中断状态寄存器 → 清中断 → 处理
        IrqEvent::HANDLED
    }
}
```

**`BlockDevice` trait**(`drivers/block/src/lib.rs`,继承 `Device`):

```rust
pub trait BlockDevice: Device {
    fn num_blocks(&self) -> u64;
    fn block_size(&self) -> usize;
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult;
    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult;
    fn flush(&self) -> DriverResult;
}
```

**`Device` trait**(`driver_base`,所有设备基类):

```rust
pub trait Device: Send + Sync {
    fn name(&self) -> &str;
    fn device_kind(&self) -> DeviceKind;
    fn irq(&self) -> Option<usize> { None }
}
```

**错误集**:`DriverError` 全集 = `AlreadyExists / WouldBlock / BadState / InvalidInput / Io / NoMemory / ResourceBusy / Unsupported`;`ResError`(device-res)全集 = `InvalidResource / MappingFailed / NoMemory / Busy / Unsupported / NoProvider`。两套错误间用 `map_err` 转换。

**设备类 → publish_* 速查**:

| 硬件层 trait | publish 调用 | DeviceKind | 能挂的 RK3588 外设 |
|--------------|-------------|-----------|--------------------|
| `BlockDevice` | `kclass::publish_block` | `Block` | eMMC/SD (dwcmshc)、SATA (ahci) |
| `NetDevice` | `kclass::publish_net` | `Net` | 以太网 |
| `CharDevice` | `kclass::publish_char` | `Char` | UART、(降级)watchdog/rtc |
| —(无) | — | — | i2c/spi controller(**blocker**) |

**读时钟频率(MMC/I2C/UART/SPI 算分频都会撞)**:Linux 用 `clk_get_rate(ciu_clk)`;x-kernel 走 `clk::devm_clk_get(device,"ciu")`(总线已预解析 `ResourceDesc::Clock`,驱动不碰 DT)。**别想当然读 DT `clock-frequency`**——Rockchip MMC 节点通常只有 `max-frequency`(卡时钟上限/caps),**不是** CIU 输入速率,不能当分频分母。`devm_clk_get` 失败/速率 None 时按 fallback(对照 U-Boot `mmc info`)。详见 `clk-subsystem.md` 与 `gaps.md` clk 条目。

**HAL 抽象模式**:当硬件逻辑层 crate(如 `drivers/net/src/fxmac`)定义了自己的 `KernelFunc` trait 需要 v2p/p2v/dma_alloc/irq 时,胶水层用 `#[crate_interface::impl_interface]` 提供宿主实现。新驱动优先直接用 `device-res` 的 `devm_*`,避免再造 HAL trait。

**早期 init 的 `log::*` 是静默的**:`khal::early_driver_init()` 早于 "Logging is enabled"。
诊断改用 `kernel_boot::bootln!("...")`(boot 控制台,日志带 `[boot]` 前缀)。
<!-- GEN-DOC:APPENDIX-END -->
