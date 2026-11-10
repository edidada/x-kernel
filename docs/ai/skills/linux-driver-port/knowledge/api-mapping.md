<!-- 本文件由 `ldp gen-doc` 从 data/api-mapping.json 生成。勿手改——改 data/api-mapping.json 后重跑 `ldp gen-doc`。 -->

# Linux → x-kernel API 映射表

Phase 2(映射)的查表依据。**每一条 x-kernel 接口都来自项目源码**(括注源码位置)。查表规则:命中 → 按「x-kernel 对应」替换;未命中 → 记缺口,转 `gaps.md`。**不要自行编造相近接口(铁律 2)。**

**命令式查询**:跑 `ldp lookup <api>` 拿结构化候选(显式截断,不全文倾倒)。

符号约定:`dev` = `&DeviceObject`/`Arc<DeviceObject>`;`DriverResult<T> = Result<T, DriverError>`;`ResResult<T> = Result<T, ResError>`。

## 1. 驱动模型与注册

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `struct platform_driver + .probe/.remove` | `impl DriverOps<PlatformBus> for XxxDriver { type Data; fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Self::Data, ProbeError>; fn remove }` | Data 即 Linux 的 `struct foo_priv`:probe 里构造并返回,框架持有,remove 原样可变取回,remove 返回后 drop(释放其中所有 RAII guard) [drivers/base/src/driver.rs:15] |
| `static const struct of_device_id xxx_of_match[] + .of_match_table` | `static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("vendor,dev")), PlatformId::Alias(AliasMatch::new("alias"))]` | OfMatch 按 compatible 匹配;AliasMatch 给组合根的 board-static 枚举用 [drivers/base/src/bus/platform_bus.rs:153] |
| `module_platform_driver(xxx) / module_init` | `driver_base::declare_driver!(PlatformBus, XxxDriver, "xxx", IDS);` 宏生成 register() 注册入口,由组合根调用 | drv/** 不直接调框架注册 API(边界规则);家族多入口用 `... => register_xxx` 命名生成函数 [drivers/base/src/driver.rs:65] |
| `platform_set_drvdata(pdev, data) / platform_get_drvdata` | `DriverOps::Data` 关联类型:probe 返回值即 drvdata,remove 收 `&mut Self::Data` | 无 untyped 指针、无 downcast;中断 handler 里用 Weak<Data> 共享 [drivers/base/src/driver.rs:15] |
| `module_pci_driver(xxx)` | `driver_base::declare_driver!(PciBus, XxxDriver, "xxx", PCI_IDS);`;PCI id 表 `PciMatch` | PciBus 与 PciMatch 定义在 drivers/bus/pci(注册 API 仍由宏生成,drv/** 不直接调) [drivers/bus/pci/src/lib.rs:125] |
| `-EPROBE_DEFER / dev_err_probe()` | `ProbeError::Deferred`(`dev.env().defer(DepSource)` 产生;如 clk provider 未就绪) | probe 泵会重新调度依赖未就绪的设备;驱动只管返回 Deferred [drivers/base/src/device.rs:455] |
| `MODULE_AUTHOR/MODULE_LICENSE/MODULE_DESCRIPTION` | 文件头 SPDX 许可注释(`// SPDX-License-Identifier: Apache-2.0` + Copyright 行) | 树卫生检查(check_header)强制 [drivers/base/src/lib.rs:1] |

## 2. 设备资源 — MMIO 寄存器

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `platform_get_resource(pdev, IORESOURCE_MEM, n)` | `dev.res().mmio(n)? -> MmioRegion`(base/size);按 reg-names 用 `mmio_by_name(name)?` | 资源由总线层从 DT/PCI BAR 预解析,驱动不碰固件 [drivers/base/src/device.rs:285] |
| `devm_platform_ioremap_resource(pdev, n) / devm_ioremap` | `dev.res().iomap(n)? -> IoMap`(RAII,drop 时 unmap);命名区域用 `iomap_by_name(name)?` | IoMap 为 devres 型句柄,存进 DriverOps::Data 即自动随设备释放 [drivers/base/src/device.rs:327] |
| `ioremap(非 devm,自管生命周期)` | `dev.res().iomap_region(region: MmioRegion, name)? -> IoMap` | 对拿到的 MmioRegion 手动映射;生命周期仍由 IoMap guard 承载 [drivers/base/src/device.rs:340] |
| `readl/writel, readb/readw/readq, writeb/writew/writeq` | `regs.read32(offset)` / `regs.write32(offset, val)`;另有 read8/16/64、write8/16/64 | offset 相对区域基址;IoMap 是 `&IoMap` 共享只读句柄,内部 volatile [drivers/base/src/devres/iomap.rs:94] |
| `ioread32/iowrite32(repit/to/副本族)` | 同上(IoMap 方法);块拷贝无封装 helper | [drivers/base/src/devres/iomap.rs:94] |
| `memcpy_fromio / memcpy_toio` | `regs.as_ptr() -> *mut u8` 后自行 volatile 拷贝(无封装 helper) | 必要时写局部 unsafe fn 并加 SAFETY:区域边界自证 [drivers/base/src/devres/iomap.rs:55] |

## 3. 设备资源 — 中断(IRQ)

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `platform_get_irq(pdev, n) / platform_get_irq_byname / of_irq_get` | `dev.res().irq(n)? -> IrqSpec` / `irq_by_name(name)?`(已解析:ResolvedVirq 或 FirmwareRoute) | IrqSpec 由总线层从 DT interrupt(s)(-names) 预解析 [drivers/base/src/device.rs:299] |
| `devm_request_irq(dev, irq, handler, flags, name, dev_id)` | `dev.res().request_irq(spec, name, handler)? -> DevIrq`(RAII);便利:`request_irq_at(n, h)` / `request_irq_by_name(name, h)` | handler 是 `Fn() -> IrqAction + Send + Sync + 'static` 闭包,自捕获上下文(典型 Weak<Data>),无需 dev_id [drivers/base/src/device.rs:353] |
| `irqreturn_t (*handler)(int irq, void *dev_id)` | `type IrqHandlerFn = Arc<dyn Fn() -> IrqAction + Send + Sync>` | 无 irq 参数(DevIrq 已绑定);共享上下文进闭包捕获 [drivers/base/src/devres/irq.rs:71] |
| `IRQ_HANDLED / IRQ_NONE / IRQ_WAKE_THREAD` | `IrqAction::Handled` / `IrqAction::NotHandled` | wake-thread 语义无线程化 IRQ,见 gaps(workqueue 下半部替代) [drivers/base/src/devres/irq.rs:49] |
| `IRQF_TRIGGER_RISING/FALLING/HIGH/LOW` | `IrqTrigger::{EdgeRising, EdgeFalling, LevelHigh, LevelLow, …}` | 触发方式由固件(DT interrupts 描述符)解析进 IrqSpec,请求时不重复指定;类型来自 kirq,driver_base 再导出 [drivers/base/src/resource.rs:156] |
| `free_irq(irq, dev_id)` | drop `DevIrq` 句柄(RAII,释放注册) | [drivers/base/src/devres/irq.rs:15] |
| `disable_irq/enable_irq(运行期屏蔽)` `missing` | 缺口:DevIrq 无 enable/disable 接口;需运行期屏蔽时上报缺口并评估 kirq 域接口 |  |
| `request_threaded_irq(中断下半部线程)` `missing` | 缺口:无线程化 IRQ。上半部 IrqAction::Handled;重活用 kworkng 工作队列延后(见 gaps workqueue 条) |  |

## 4. 设备资源 — DMA

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `dmam_alloc_coherent / dma_alloc_coherent` | `dev.res().alloc_coherent(len, align)? -> CoherentMem` | CoherentMem { cpu 指针, bus_addr, len };bus_addr 是设备可见地址(DMA 寄存器里写它) [drivers/base/src/device.rs:388] |
| `dma_free_coherent` | drop `CoherentMem`(RAII) | [drivers/base/src/devres/dma.rs:12] |
| `dma_handle(设备可见总线地址)` | `CoherentMem::bus_addr`(u64,设备可见) | [drivers/base/src/devres/dma.rs:12] |
| `dma_map_single / dma_unmap_single(streaming 映射)` `missing` | 缺口:无 streaming 映射 API;用 coherent 缓冲替代或上报缺口 |  |
| `dma_sync_single_for_cpu/device` `missing` | 缺口:随 streaming 映射一并缺;coherent 内存无需 sync |  |
| `dma_set_mask_and_coherent(dev, DMA_BIT_MASK(n))` | 无显式掩码 API;RK3588 aarch64 默认按 64-bit 处理,受限掩码需求上报 | [drivers/base/src/device.rs:388] |

## 5. 设备类发布(把驱动实例挂进系统)

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `register_netdev / alloc_etherdev + net_device_ops` | `impl subsys_net::NetDevice for …` + `dev.publish(&subsys_net::class::NET, Arc::new(dev), "name")? -> Publication<NetDeviceImpl>` | Publication 存进 Data;drop 即反发布 [drivers/subsystems/net/src/class.rs:20] |
| `block_device_operations + add_disk/gendisk` | `impl subsys_block::BlockDevice for …` + `dev.publish(&subsys_block::class::BLOCK, Arc::new(dev), "name")?` | [drivers/subsystems/block/src/class.rs:47] |
| `misc_register / cdev_init+add / tty_register_driver` | `impl subsys_char::CharDevice for …` + `dev.publish(&subsys_char::class::CHAR, Arc::new(dev), "name")?` | [drivers/subsystems/char/src/class.rs:17] |
| `devm_rtc_device_register / rtc_class_ops` | `impl subsys_rtc::RtcDevice for …` + `dev.publish(&subsys_rtc::class::RTC, Arc::new(dev), "name")?` | rtc 契约方法返回 kerrno::KResult [drivers/subsystems/rtc/src/class.rs:24] |
| `input/hwrng/display/vsock 等子系统注册` | 对应 `drivers/subsystems/<class>` 契约:`drivers/subsystems/{input,hwrng,display,vsock}` 各有 class:: 静态 + 设备 trait | [drivers/subsystems/hwrng/src/lib.rs:1] |
| `i2c/spi/regulator/watchdog 子系统注册` `missing` | 缺口:无对应 class 契约与 DeviceKind;见 gaps |  |
| `platform_device_register(板级静态设备)` | 组合根以 board-static(PortOrigin::BoardStatic)枚举 + `PlatformId::Alias(AliasMatch::new("xxx"))` 匹配;一般无需驱动侧操作 | [drivers/base/src/bus/platform_bus.rs:30] |

## 6. 同步与并发

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `spinlock_t + spin_lock_irqsave/spin_unlock_irqrestore` | `kspin::SpinNoIrq<T>`(`SpinNoIrq::new(t)`,.lock() 取 guard,drop 释放) | [task/kspin/src/lib.rs:144] |
| `spin_lock(仅防抢占)` | `kspin::SpinNoPreempt<T>` | [task/kspin/src/lib.rs:135] |
| `raw_spinlock_t` | `kspin::SpinNoPreempt<T>`(v2 无单独 raw 层;guard 类型即关抢占/关中断语义) | [task/kspin/src/lock.rs:46] |
| `rwlock_t` | `kspin::SpinRwNoIrq<T>`(.read()/.write()) | [task/kspin/src/lib.rs:158] |
| `DEFINE_MUTEX / mutex_lock/mutex_unlock` | `ksync::Mutex<T>`(.lock() 取 guard) | [task/ksync/src/mutex.rs:223] |
| `rw_semaphore(可睡眠读写锁)` | `ksync::RwLock<T>`(.read()/.write()) | [task/ksync/src/rwlock.rs:231] |
| `struct semaphore / down/up` | `ksync::Semaphore::new(n)`(.acquire()/.try_acquire()) | [task/ksync/src/semaphore.rs:15] |
| `atomic_t / atomic_set/atomic_read/atomic_inc/…` | `core::sync::atomic::{AtomicU32, AtomicUsize, AtomicBool, …}` + Ordering | 项目规范:避免随手原子;仅在确需无锁计数/标志时用 |
| `wait_event*/wait_queue_head_t/wake_up*` | `ktask::WaitQueue`(.wait()/.wait_until(closure)/.wait_timeout(dur)/.notify_one(true)/.notify_all(true)) | [task/ktask/src/wait_queue.rs:38] |
| `struct completion / wait_for_completion/complete` `workaround` | 无直接等价 → 用 WaitQueue 替代(缺口记录) | [task/ktask/src/wait_queue.rs:38] |
| `INIT_WORK / schedule_work / queue_work` | `kworkng::NormalQueue::new(name, QueueConfig)` 建队列;工作项 `WorkHandle` 句柄 queue/queue_on 到队列 | 系统默认队列与专用队列都可;驱动一般 own 一个 NormalQueue 或用共享默认队列 [task/kworkng/src/normal_queue.rs:22] |
| `INIT_DELAYED_WORK / schedule_delayed_work / mod_delayed_work` | `kworkng::DelayedWorkHandle::new(callback)`;.queue/.modify/.cancel_pending/.cancel_sync/.flush | [task/kworkng/src/delayed_work_handle.rs:24] |
| `cancel_work_sync / flush_work / destroy_workqueue` | kworkng 句柄方法:.cancel_sync()/.flush()/.shutdown()(队列级) | [task/kworkng/src/delayed_work_handle.rs:154] |
| `RCU (rcu_read_lock/call_rcu/…)` `workaround` | 缺口:无 RCU;用 Arc/RwLock 替代并记录 |  |

## 7. 内存分配

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `kmalloc/kzalloc + kfree` | `Box::new(x)` / `Vec::new()` / `alloc::vec![]`;释放由 drop 完成 |  |
| `devm_kmalloc/devm_kzalloc/devm_kfree` | `Box::new(x)` 并把所有权存进 `DriverOps::Data`(随绑定释放);需要设备级清理钩子时用 devres 注册(`register_cleanup` 类接口) |  |
| `kcalloc / kcalloc_array(清零数组)` | `vec![T::default(); n]` 或 `Vec::with_capacity(n)` |  |
| `kmem_cache_create + kmem_cache_alloc(slab)` `missing` | 缺口:无 slab cache;用 Box/Vec,性能敏感路径上报缺口 |  |
| `get_zeroed_page / __get_free_page / alloc_pages` | `kalloc` 页分配器:`KAlloc::alloc_pages(num_pages, align_pow2, UsageKind)` -> 页帧 | 整页粒度;小对象用 Box/Vec [mm/kalloc/src/lib.rs:571] |
| `vmalloc(非连续物理大缓冲)` `workaround` | 缺口:无 vmalloc 等价;大缓冲用 DMA coherent(alloc_coherent)或上报缺口 |  |
| `kmap/kunmap/kmap_atomic` | 无(aarch64 直接访问物理地址映射,无需临时映射) |  |
| `copy_to_user/copy_from_user` | 驱动面无用户拷贝 API;char 设备契约方法直接收 `&mut [u8]`/`&[u8]`(用户拷贝由 class/系统调用层负责) | [drivers/subsystems/char/src/lib.rs:26] |

## 8. 时间、延迟与定时

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `msleep(ms) / schedule_timeout_interruptible` | `ktask::sleep(ktime_types::TimeSpan::from_millis(ms))` | 参数是 ktime_types::TimeSpan,不是 core::time::Duration [task/ktask/src/api.rs:733] |
| `usleep_range(min, max)` | `ktask::sleep(TimeSpan::from_micros(..))` | [task/ktask/src/api.rs:733] |
| `udelay / mdelay(忙等,不可睡眠上下文)` | `khal::time::busy_wait(TimeSpan)` / `busy_wait_until(MonotonicInstant)` | 中断上下文/必须原地等时用;能用 ktask::sleep 就别忙等 [arch/khal/src/time/mod.rs:124] |
| `ktime_get / ktime_get_ns / ktime_get_real` | `khal::time::monotonic_time_nanos()` / `monotonic_time() -> MonotonicInstant` | [arch/khal/src/time/mod.rs:101] |
| `jiffies / msecs_to_jiffies / time_after/time_before` | `MonotonicInstant` 直接比较(无需 jiffies 换算);`ktime_types` 提供 TimeSpan 换算 | [arch/khal/src/time/mod.rs:101] |
| `timer_setup / mod_timer / del_timer(软定时器)` | `ktimer-core`:`TimerSet` + `Timer::new(&set, handler)`,.rearm(deadline)/.try_cancel()/.cancel_sync() | 回调在定时器软中断上下文;延后到任务上下文再走 kworkng [task/ktimer-core/src/timer.rs:59] |
| `hrtimer(高精度定时器)` `workaround` | 无独立 hrtimer 框架;ktimer-core 承担软定时语义,精度需求上报 | [task/ktimer-core/src/lib.rs:1] |

## 9. 日志与错误处理

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `dev_info/dev_err/dev_warn/dev_dbg` | `log::info!/error!/warn!/debug!`(无 dev_ 变体;消息里拼 `dev.info().path()`) |  |
| `printk / pr_info/pr_err/pr_warn` | `log::info!` 等 |  |
| `-ENOMEM` | `RuntimeError::NoMemory`(包进 ProbeError::Fatal 或 map 到 KError) | [drivers/base/src/error.rs:109] |
| `-EINVAL` | `RuntimeError::Invalid` | [drivers/base/src/error.rs:110] |
| `-ENODEV/-ENXIO` | `RuntimeError::NotFound` 或 `BadState`(视语义) | [drivers/base/src/error.rs:117] |
| `-EBUSY` | `RuntimeError::Busy` | [drivers/base/src/error.rs:112] |
| `-EIO` | `RuntimeError::Io`(类契约里是 `kerrno::KError::Io`) | [drivers/base/src/error.rs:113] |
| `-ENOTSUPP/-EOPNOTSUPP` | `RuntimeError::Unsupported` | [drivers/base/src/error.rs:115] |
| `-EEXIST` | `RuntimeError::AlreadyExists` | [drivers/base/src/error.rs:116] |
| `错误传播(返回负 errno + IS_ERR/PTR_ERR)` | `?` 运算符 + `Result<T, ProbeError>` / `KResult<T>`;两套错误间用 map_err 转换 | [drivers/base/src/error.rs:76] |

## 10. 设备树属性读取

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `of_property_read_u32(node, "x", &val)` | `dev.fw().u32("x")? -> Option<u32>`;原始节点路径:`of::property_u32(node, "x")` | 驱动优先走 fw()(设备绑定的节点快照);of:: 直接查询留给总线/组合根 [drivers/base/src/device.rs:429] |
| `of_property_read_string(node, "x", &s)` | `dev.fw().string("x") -> Option<&'static str>`;底层走 of::fdt() 的 FdtNode property 接口 | [drivers/base/src/device.rs:434] |
| `of_property_read_bool(node, "x")` | `dev.fw().flag("x") -> bool` | [drivers/base/src/device.rs:442] |
| `of_find_compatible_node(NULL, NULL, "v,d")` | `of::find_compatible("vendor,dev") -> Option<FdtNode>` | [drivers/firmware/of/src/lib.rs:509] |
| `of_find_node_by_name / of_find_node_by_path` | `of::find_node("/path")`(支持别名解析) | [drivers/firmware/of/src/lib.rs:525] |
| `clk_get(dev, name) / devm_clk_get + clk_get_rate` | `dev.res().clk("name")? -> ClkHandle`(.get_rate()/.prepare_enable()/.set_rate());缺 provider 时返回 ProbeError::Deferred | v2 已落地:clk 经 driver-host 桥接 core/kclk;消费端只见 ClkHandle [drivers/base/src/device.rs:396] |
| `of_get_named_gpio / gpiod_get` `missing` | 缺口:无 gpio 子系统;reset-gpios 等需求上报(见 gaps gpio 条) |  |
| `of_irq_get / of_irq_count` | `dev.res().irq(n)?`(已解析 IrqSpec);原始接口 `of::first_interrupt_desc(node)` | [drivers/firmware/of/src/lib.rs:683] |
| `of_address_to_resource(pdev->dev.of_node, index, &res)` | `dev.res().mmio(index)?`(已解析 MmioRegion) | [drivers/base/src/device.rs:285] |
| `of_match_node(of_match_table, node)` | 总线层用 `PlatformId::Of(OfMatch)` id 表自动匹配,驱动不手动 match | [drivers/base/src/bus/platform_bus.rs:178] |

## 11. 地址转换(DMA / 物理-虚拟)

| Linux | x-kernel 对应 | 备注 / 位置 |
|-------|---------------|-------------|
| `virt_to_phys` | `khal::mem::v2p(VirtAddr) -> PhysAddr` | [arch/khal/src/mem/mod.rs:38] |
| `phys_to_virt` | `khal::mem::p2v(PhysAddr) -> VirtAddr` | [arch/khal/src/mem/mod.rs:33] |
| `PAGE_SIZE / PAGE_SHIFT` | `khal::mem::PAGE_SIZE_4K` | [arch/khal/src/mem/mod.rs:13] |
| `物理/虚拟地址类型(phys_addr_t/dma_addr_t)` | `khal::mem::{PhysAddr, VirtAddr}`(memaddr 类型族);DMA 设备地址用 CoherentMem::bus_addr(u64) | [arch/khal/src/mem/mod.rs:13] |

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
