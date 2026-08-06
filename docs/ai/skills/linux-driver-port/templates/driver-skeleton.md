# 驱动骨架模板

Phase 3（生成）填空用。括号 `<…>` 是占位符。两个文件 + 注册三连。

> 先读 [`driver-anatomy.md`](../knowledge/driver-anatomy.md) 搞清双层结构。硬件寄存器/bit/序列必须照搬 Linux（铁律 1），`<…>` 只填"项目相关"的部分（路径、名字、trait）。

---

## 文件 1：硬件逻辑层 `drivers/<class>/src/<name>.rs`

以 Block 设备为例；Net/Char 换对应 trait 即可（见 [api-mapping §3](../knowledge/api-mapping.md)）。

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! <DRIVER_NAME>: <一句话描述，注明移植自 Linux drivers/<...>/<file>.c>

#![allow(unsafe_op_in_unsafe_fn)]

use core::ptr::NonNull;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;

use crate::BlockDevice; // Net → NetDevice；Char → char_driver::CharDevice

// ── 寄存器布局（照搬 Linux 的 #define / BITS，铁律 1） ──────────────────
const REG_CTRL:   usize = 0x00;
const REG_STATUS: usize = 0x04;
const CTRL_ENABLE: u32 = 1 << 0;
const CTRL_RESET:  u32 = 1 << 1;
// …

/// 设备私有数据（≈ Linux struct <name>_priv）。
pub struct <Name>Driver {
    base: usize,                       // 映射后的 vaddr
    inner: SpinNoIrq<()>,              // 串行化硬件访问；锁内放可变状态
    // num_blocks / ring / ctx …
}

impl <Name>Driver {
    /// 创建并初始化设备。
    ///
    /// # Safety
    /// `base` 必须是 `<controller>` 寄存器块**独占映射**的虚拟地址，
    /// 生命周期不短于返回的 `Self`，且无并发访问。
    pub unsafe fn new(base: usize) -> DriverResult<Self> {
        let dev = Self { base, inner: SpinNoIrq::new(()) };
        dev.hw_init()?;
        Ok(dev)
    }

    /// 读一个 32-bit 寄存器。
    #[inline]
    unsafe fn rd32(&self, off: usize) -> u32 {
        (self.base as *const u32).add(off / 4).read_volatile()
    }
    #[inline]
    unsafe fn wr32(&self, off: usize, v: u32) {
        (self.base as *mut u32).add(off / 4).write_volatile(v);
    }

    /// 硬件初始化序列（照搬 Linux probe 的寄存器写法，铁律 1）。
    fn hw_init(&self) -> DriverResult<()> {
        // SAFETY: 仅在 new() 中调用，此时 base 独占有效。
        unsafe {
            self.wr32(REG_CTRL, CTRL_RESET);
            // udelay(100) → 见 gaps.md 的忙等替代（铁律 4：记录缺口）
            self.wr32(REG_CTRL, CTRL_ENABLE);
        }
        let _g = self.inner.lock();
        Ok(())
    }
}

impl Device for <Name>Driver {
    fn name(&self) -> &str { "<name>" }
    fn device_kind(&self) -> DeviceKind { DeviceKind::Block } // Net/Char 按需
}

impl BlockDevice for <Name>Driver {
    fn num_blocks(&self) -> u64 { /* … */ }
    fn block_size(&self) -> usize { 512 }
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let _g = self.inner.lock();
        // SAFETY: base 在 Self 生命周期内独占有效。
        // …寄存器操作 + 数据搬运…
        let _ = (block_id, buf);
        Ok(())
    }
    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult { /* … */ }
    fn flush(&self) -> DriverResult { Ok(()) }
}
```

> 中断处理：若设备有 IRQ，把 `IrqHandler` impl 放这里或胶水层（见文件 2 的 `devm_request_irq`）。中断里只清中断+置标志+唤醒，重活交线程（[gaps.md workqueue](../knowledge/gaps.md)）。

---

## 文件 2：胶水层 `drivers/kdriver/src/driver_registry/<bus>/<name>.rs`

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use driver_base::DeviceKind;
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::{BoxedDriver, firmware_specs::<NAME>_SPEC};

struct <Name>Drv;

impl DeviceDriver for <Name>Drv {
    fn name(&self) -> &'static str { "<name>" }
    fn device_kind(&self) -> DeviceKind { DeviceKind::Block }
    fn bus_types(&self) -> &'static [BusTypeId] { &[BusTypeId::PLATFORM] } // 或 PCI
    fn matcher(&self) -> &dyn DeviceMatcher { &<NAME>_SPEC }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        // 1) 取 MMIO（vaddr 模式，推荐）
        let (vaddr, _size) = crate::iomap_first_mmio(device.as_ref(), "<name>")?;
        let vaddr = vaddr.as_ptr() as usize;

        // 2)（可选）注册中断
        // use device_res::{devm_request_irq, IrqEvent};
        // if let Some(irq) = device.first_irq() {
        //     crate::devm_request_irq(device.as_ref(), irq, Arc::new(|| IrqEvent::HANDLED))?;
        // }

        // 3) 实例化硬件层
        // SAFETY: vaddr 是该设备独占映射的 MMIO 窗口，生命周期随 device。
        let dev = unsafe { <class>::<name>::<Name>Driver::new(vaddr)? };

        // 4) 发布到设备类
        kclass::publish_block(device, Box::new(dev)) // publish_net / publish_char
    }
}

pub(super) fn descriptor() -> BoxedDriver { Arc::new(<Name>Drv) }
```

> 若硬件层走 **paddr 模式**（如 fxmac）：把 `iomap_first_mmio` 换成
> `let mmio = crate::first_mmio_resource(device.as_ref())?;`
> 再 `let dev = <Name>Nic::init(mmio.base);`。新驱动优先 vaddr 模式。

---

## 注册三连改动

### ① `driver_registry/firmware_specs.rs`

```rust
#[cfg(feature = "<name>")]
pub(crate) const <NAME>_SPEC: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "<name>",
    dt_compatibles: &["<vendor>,<compat>"], // ← 来自 Linux of_device_id
    acpi_ids: &[],
};
```

### ② `driver_registry/<bus>/mod.rs`

```rust
#[cfg(feature = "<name>")]
mod <name>;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    // …existing…
    #[cfg(feature = "<name>")]
    <name>::descriptor,
];
```

### ③ `drivers/kdriver/Cargo.toml` + 平台 defconfig

```toml
[features]
<name> = []
```
在 `platforms/kplat-aarch64/rk3588_defconfig` 加 `KFEAT_<NAME>=y`（参考 [build-workflow](../../build-workflow/SKILL.md)）。

---

## 填空检查表

- [ ] 寄存器常量/bit 与 Linux `#define` 逐一核对（铁律 1）
- [ ] 初始化序列顺序、延时、`udelay` 替代与 Linux 一致
- [ ] `unsafe` 块都有 `// SAFETY:` 注释（铁律 3，[code-guidelines](../../code-guidelines/SKILL.md)）
- [ ] `new()` 有 `# Safety` 文档
- [ ] 用到的每个 x-kernel API 都在 [api-mapping.md](../knowledge/api-mapping.md) 有据可查（铁律 2）
- [ ] 未映射的依赖全部进了 [gap-report](gap-report.md)（铁律 4）
- [ ] 设备类 trait 与 `publish_*` 对应正确
- [ ] 注册三连完整（firmware_specs / mod.rs / Cargo.toml + defconfig）
