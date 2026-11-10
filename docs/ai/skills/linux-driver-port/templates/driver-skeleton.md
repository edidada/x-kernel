# 标准 Rust 驱动骨架(v2 driver model,可直接填空)

对照 Linux `drivers/xxx/foo.c`(platform 驱动)转写。双层结构见
`knowledge/driver-anatomy.md`;落点:`drivers/drv/<family>/src/<name>.rs`(具体驱动)+ 类契约在 `drivers/subsystems/<class>`(已有则直接消费)。

## 文件:驱动(绑定 + 硬件逻辑同 crate)

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! <NAME> driver:译自 Linux `drivers/<xxx>/foo.c`(linux-7.1.8)。
//!
//! 硬件序列/位段忠实翻译;框架胶水按 v2 设备模型重写。
//! 参考序列:<编号列出 reference 序列清单,见 workflow P1>。

use alloc::sync::Arc;

use driver_base::{
    Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, Publication,
};

/// `static const struct of_device_id foo_of_match[]`
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("vendor,foo"))];

// ---- 寄存器布局(全部命名,注释锚参考源文件行号) --------------------------
const FOO_CTRL: usize = 0x00;
const FOO_STAT: usize = 0x04;
// 位段进 data/bitfield-defs.json,由 ldp check-layout 守护(铁律 1/2)。
const CTRL_START: u32 = 1 << 0;

/// 硬件逻辑:寄存器访问 + 初始化序列 + 中断处理。只认 IoMap/基址,
/// 不依赖框架(可宿主机单测);协议状态机复杂时拆独立模块 + access trait。
struct FooHw {
    regs: IoMap,
}

impl FooHw {
    fn init(&mut self) -> Result<(), ProbeError> {
        let ctrl = self.regs.read32(FOO_CTRL);
        // 忠实翻译 Linux foo_probe 的初始化序列(不做"优化",不合并步骤)
        self.regs.write32(FOO_CTRL, ctrl | CTRL_START);
        Ok(())
    }
}

/// 类契约实现(样例:char;按设备实际类换 subsys_block::BlockDevice 等)
struct FooDevice {
    hw: kspin::SpinNoIrq<FooHw>, // 中断上下文共享 → SpinNoIrq
}

impl subsys_char::CharDevice for FooDevice { /* read/write/poll … */ }

/// DriverOps::Data:Linux 的 struct foo_priv。guard 顺序:
/// Publication 第一,其后按逆序释放(资源比发布活得久)。
pub struct FooData {
    _publication: Publication<subsys_char::CharDeviceImpl>,
    _irq: driver_base::DevIrq,
}

pub struct FooDriver;

impl DriverOps<PlatformBus> for FooDriver {
    type Data = FooData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<FooData, ProbeError> {
        // devm_platform_ioremap_resource(pdev, 0)
        let regs = dev.res().iomap(0)?;
        // devm_clk_get + clk_prepare_enable(缺 provider → Deferred)
        let _clk = dev.res().clk("foo")?;
        _clk.prepare_enable()?;

        let hw = FooHw { regs };
        hw.init()?;

        let device = Arc::new(FooDevice { hw: kspin::SpinNoIrq::new(hw) });

        // register_xxx / misc_register / add_disk(按实际类换 class 静态)
        let publication = dev.publish(&subsys_char::class::CHAR, device, "foo")?;

        // devm_request_irq:闭包自捕获上下文(典型 Weak<FooDevice>)
        let weak = Arc::downgrade(&shared);
        let irq = dev.res().request_irq_at(0, move || {
            match weak.upgrade() {
                Some(dev) => { dev.poll(); driver_base::IrqAction::Handled }
                None => driver_base::IrqAction::NotHandled,
            }
        })?;

        log::info!("foo: probed at {:#x}", base);
        Ok(FooData { _publication: publication, _irq: irq })
    }

    fn remove(&self, dev: &mut Device<'_, PlatformBus>, data: &mut FooData) {
        // quiesce 硬件;guard 释放由框架在 remove 返回后按逆序完成
        let _ = (dev, data);
    }
}

// module_platform_driver(foo_driver)
driver_base::declare_driver!(PlatformBus, FooDriver, "foo", IDS);
```

## 接线清单(组合根侧)

1. `drivers/drv/<family>/Cargo.toml`:crate 声明 + 依赖(`driver-base` + 对应 `subsys-<class>`;`drv/**` 不得依赖宿主侧 crate,见 `check_boundaries`)。
2. 组合根(`drivers/init`)在 initcall 序列调用 `drv_<family>::foo::register()`(由宏生成)。
3. Kconfig:平台 defconfig 里启用对应 feature(`cp platforms/<plat>/<plat>_defconfig .config && make defconfig`)。

## 纪律提醒(违反即返工)

- **铁律 1/2**:寄存器序列/位段/magic number 逐位对照参考源码;位段声明进 `bitfield-defs.json` 并跑 `ldp check-layout`。
- **铁律 3**:资源全部 RAII(`IoMap`/`DevIrq`/`ClkHandle`/`CoherentMem`/`Publication`);每个 `unsafe` 前置 `// SAFETY:`。
- **铁律 4**:无法映射的依赖(phy/reset/gpio/regulator/syscon/MSI…)进缺口报告,不得静默 stub。
