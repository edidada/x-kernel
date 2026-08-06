# x-kernel 驱动解剖：双层结构 + 注册三连

开始移植前必读。理解 x-kernel 驱动是怎么"挂"进系统的，才能知道一个 Linux 驱动要被拆成哪几块、落到哪些目录。

## 核心心智模型：Linux 单文件 vs x-kernel 双层

Linux 里一个 `drivers/xxx/foo.c` 通常**一个文件**既做总线注册（`module_platform_driver`、`of_match_table`），又做硬件逻辑（寄存器、中断、协议）。x-kernel 把这两层**强制分离**：

```
┌─────────────────────────────────────────────────────────────┐
│ 硬件逻辑层  drivers/<class>/src/<name>.rs                    │
│   纯硬件：寄存器读写、初始化序列、中断处理、协议状态机        │
│   不依赖 DeviceObject / kclass —— 只认一个基址（vaddr 或 Io） │
│   实现 <Class>Device trait（BlockDevice/NetDevice/CharDevice）│
└─────────────────────────────────────────────────────────────┘
                         ▲  new(vaddr) / init(base) / Hal trait
                         │  （由胶水层调用）
┌─────────────────────────────────────────────────────────────┐
│ 胶水层  drivers/kdriver/src/driver_registry/<bus>/<name>.rs  │
│   实现 DeviceDriver trait：name / device_kind / bus_types /  │
│   matcher(→FirmwareMatchSpec) / probe_device                 │
│   probe_device 里：取 MMIO/IRQ → new 硬件层 → kclass::publish│
│   pub(super) fn descriptor() -> BoxedDriver                  │
└─────────────────────────────────────────────────────────────┘
```

**为什么分两层**：硬件逻辑层可被多总线复用、可单测、可移植；胶水层薄而统一，只管"发现设备 → 拿资源 → 实例化 → 发布"。

## 胶水层标准形态（照抄即可）

来自 [driver_registry/block/sdmmc.rs](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs)（最简洁的样例）：

```rust
use alloc::{boxed::Box, sync::Arc};
use driver_base::DeviceKind;
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};
use crate::driver_registry::{BoxedDriver, firmware_specs::SDMMC};

struct SdmmcDriver;

impl DeviceDriver for SdmmcDriver {
    fn name(&self) -> &'static str { "sdmmc" }
    fn device_kind(&self) -> DeviceKind { DeviceKind::Block }
    fn bus_types(&self) -> &'static [BusTypeId] { &[BusTypeId::PLATFORM] }
    fn matcher(&self) -> &dyn DeviceMatcher { &SDMMC }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, _) = crate::iomap_first_mmio(device.as_ref(), "sdmmc")?;
        let vaddr = vaddr.as_ptr() as usize;
        // SAFETY: vaddr 是该设备独占映射的 MMIO 窗口，生命周期随 device。
        let dev = unsafe { block::sdmmc::SdMmcDriver::new(vaddr) };
        kclass::publish_block(device, Box::new(dev))
    }
}

pub(super) fn descriptor() -> BoxedDriver { Arc::new(SdmmcDriver) }
```

**probe_device 的固定四步**：

1. 取资源：`iomap_first_mmio(dev, "name")` → `(NonNull<u8>, size)`；中断用 `dev.first_irq()` + `devm_request_irq`。
2. 实例化硬件层：把 vaddr（或 paddr）交给 `XxxDriver::new(...)`。
3. 发布：`kclass::publish_<class>(device, Box::new(dev))?`。
4. 错误用 `?` 传播（`DriverResult`）。

### 两种取址模式

| 模式 | 胶水层 | 硬件层构造 | 代表 |
|------|--------|-----------|------|
| **vaddr 模式**（推荐） | `iomap_first_mmio` 拿 vaddr，传给硬件层 | `unsafe fn new(vaddr: usize)`，内部用裸指针或自建 `Io` | sdmmc |
| **paddr 模式** | `first_mmio_resource` 拿 `MmioRegion.base`（物理），传给硬件层 | 硬件层自己 v2p/p2v 或映射 | fxmac |

> 新驱动优先 vaddr 模式：胶水层 `iomap_first_mmio` 已做映射+边界检查，硬件层接 `usize` vaddr，`probe_device` 内 `unsafe` 边界清晰。

### HAL 桥接（仅当复用既有硬件 crate 时）

某些硬件逻辑 crate（如 `drivers/net/src/fxmac`）定义了自己的 `KernelFunc` trait，需要宿主提供 v2p/p2v/dma_alloc/irq。胶水层用 `crate_interface` 提供实现，见 [net/fxmac.rs:14-43](../../../../../drivers/kdriver/src/driver_registry/net/fxmac.rs#L14-L43)。**新驱动不要造 HAL trait**——直接用 `device-res` 的 `devm_*`。

## 硬件逻辑层标准形态

来自 [block/src/sdmmc.rs](../../../../../drivers/block/src/sdmmc.rs)（BlockDevice 实现）：

```rust
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use crate::BlockDevice;

pub struct SdMmcDriver {
    inner: SpinNoIrq<SdMmc>,   // 用自旋锁串行化硬件访问，暴露 &self 接口
    num_blocks: u64,            // 缓存容量，避免每次查询加锁
}

impl SdMmcDriver {
    /// # Safety
    /// `base` 必须是该 SD/MMC 控制器寄存器块的合法映射地址，且无并发访问。
    pub unsafe fn new(base: usize) -> Self { /* … */ }
}

impl Device for SdMmcDriver {
    fn name(&self) -> &str { "sdmmc" }
    fn device_kind(&self) -> DeviceKind { DeviceKind::Block }
}

impl BlockDevice for SdMmcDriver {
    fn num_blocks(&self) -> u64 { self.num_blocks }
    fn block_size(&self) -> usize { SdMm::BLOCK_SIZE }
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult { /* 锁 → 操作 */ }
    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult { /* … */ }
    fn flush(&self) -> DriverResult { Ok(()) }
}
```

要点：
- `unsafe fn new(base)` 带 `# Safety` 文档（[code-guidelines](../../code-guidelines/SKILL.md) 要求）。
- 硬件访问串行化：用 `SpinNoIrq<T>`（关中断自旋，最常用）或 `SpinNoPreempt`。
- 实现 `Device`（name/kind）+ 设备类 trait（`BlockDevice`/`NetDevice`/`CharDevice`）。
- **寄存器操作原样照搬 Linux**：bit 位、偏移、初始化序列、延时——这些是硬件真相（铁律 1）。若硬件层接 vaddr，直接 `Io::map` 或对裸指针 `read_volatile/write_volatile`。

## 注册三连（让系统发现并加载驱动）

移植一个驱动，除两个 .rs 文件外，还要改**三处**：

### ① compatible 声明 — `driver_registry/firmware_specs.rs`

```rust
#[cfg(feature = "my-drv")]
pub(crate) const MY_DRV: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "my-drv",
    dt_compatibles: &["vendor,my-drv", "vendor,my-drv-v2"],  // 来自 Linux of_match_table
    acpi_ids: &[],
};
```
样例：[firmware_specs.rs](../../../../../drivers/kdriver/src/driver_registry/firmware_specs.rs)。

### ② 工厂注册 — `driver_registry/<bus>/mod.rs`

```rust
#[cfg(feature = "my-drv")]
mod my_drv;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    // …
    #[cfg(feature = "my-drv")]
    my_drv::descriptor,
];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
```
样例：[block/mod.rs](../../../../../drivers/kdriver/src/driver_registry/block/mod.rs)。注册入口由 [driver_registry/mod.rs](../../../../../drivers/kdriver/src/driver_registry/mod.rs) 的 `register_platform_drivers` 汇总，新 bus 不需要动它。

### ③ Cargo feature — `drivers/kdriver/Cargo.toml`

```toml
[features]
my-drv = []
```
再在平台 defconfig（如 `platforms/kplat-aarch64/rk3588_defconfig`）里 `KFEAT_…=y` 启用（参考 [`build-workflow`](../../build-workflow/SKILL.md) 的 feature→KFEAT 映射）。

> 三连缺一不可：**没 ① 系统不知道匹配哪个 compatible；没 ② 工厂不注册；没 ③ 编译不进镜像。**

## 设备类 → publish_* 速查

| 硬件层 trait | publish 调用 | DeviceKind | 能挂的 RK3588 外设 |
|--------------|-------------|-----------|--------------------|
| `BlockDevice` | `kclass::publish_block` | `Block` | eMMC/SD (dwcmshc)、SATA (ahci) |
| `NetDevice` | `kclass::publish_net` | `Net` | 以太网 |
| `CharDevice` | `kclass::publish_char` | `Char` | UART、（降级）watchdog/rtc |
| —（无） | — | — | i2c/spi controller（**blocker**，见 [gaps.md](gaps.md)） |

> char 类的发布样例见 [char/serial.rs](../../../../../drivers/kdriver/src/driver_registry/char/serial.rs)（含 stdout/auxiliary 角色分发、一个文件多个 `FirmwareMatchSpec`）。

## 速记：一个新驱动的落点清单

- [ ] `drivers/<class>/src/<name>.rs` — 硬件逻辑层（寄存器/中断/协议 + `<Class>Device` trait）
- [ ] `drivers/<class>/src/lib.rs` — `pub mod <name>;`（按 feature gate）
- [ ] `drivers/kdriver/src/driver_registry/<bus>/<name>.rs` — 胶水层
- [ ] `drivers/kdriver/src/driver_registry/<bus>/mod.rs` — mod + DRIVER_FACTORIES
- [ ] `drivers/kdriver/src/driver_registry/firmware_specs.rs` — compatible
- [ ] `drivers/kdriver/Cargo.toml` — feature
- [ ] 平台 defconfig — `KFEAT_…=y`
- [ ] 缺口报告 — 缺失子系统清单
