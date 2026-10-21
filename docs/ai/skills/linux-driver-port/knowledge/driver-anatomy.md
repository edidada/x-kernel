# x-kernel(v2 driver model)驱动解剖:绑定 + 契约发布

开始移植前必读。理解 x-kernel 驱动是怎么"挂"进系统的,才能知道一个 Linux 驱动要被拆成哪几块、落到哪些目录。

> 本文所有代码均摘自**仓库真实驱动**(`drivers/drv/rtc/src/ls7a.rs`,RTC),链接可点开对照。

## 核心心智模型:Linux 单文件 vs x-kernel 双层

Linux 里一个 `drivers/xxx/foo.c` 通常**一个文件**既做总线注册(`module_platform_driver`、`of_match_table`),又做硬件逻辑(寄存器、中断、协议)。x-kernel 把这两层**分离**:

```
┌──────────────────────────────────────────────────────────────┐
│ 硬件逻辑层  drivers/drv/<family>/src/<name>.rs                │
│   纯硬件:寄存器读写、初始化序列、中断处理、协议状态机           │
│   只认 IoMap/MMIO 与总线无关的参数 —— 可单测、可复用           │
│   实现 <Class> 契约 trait(RtcDevice/BlockDevice/NetDevice…)   │
└──────────────────────────────────────────────────────────────┘
                    ▲ probe 里:res().iomap → new → publish(&CLASS)
                    │
┌──────────────────────────────────────────────────────────────┐
│ 胶水/绑定  同一个驱动单元内的 DriverOps impl + id 表            │
│   DriverOps<PlatformBus>::probe(&mut Device<PlatformBus>):    │
│   取资源 → 实例化 → Device::publish(&class::CLASS, …)          │
│   declare_driver!(…) 生成注册入口                              │
└──────────────────────────────────────────────────────────────┘
```

**v2 的边界规则**(`docs/ai/driver-subsystem-boundaries.md`,硬约束):
- `driver-base` 不命名任何设备家族;类契约(host-free)在 `drivers/subsystems/<class>`;
- 具体驱动在 `drivers/drv/<family>`,资源只经 `Device` 获取、成果只经
  `Device::publish::<C>` 发布;`drv/**` 不调用 `kirq` 注册 API;
- 进程全局接线(服务、资源 provider)只有一处:`driver_host::Host`。

## 绑定层标准形态(照抄即可)

来自 [drivers/drv/rtc/src/ls7a.rs](../../../../../drivers/drv/rtc/src/ls7a.rs):

```rust
use driver_base::{
    AliasMatch, Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, Publication,
};
use subsys_rtc::{RtcDevice, RtcDeviceImpl, class::RTC};

/// `static const struct of_device_id ls7a_rtc_of_match[]` + 板级静态别名
static IDS: &[PlatformId] = &[
    PlatformId::Of(OfMatch::compatible("loongson,ls7a-rtc")),
    PlatformId::Alias(AliasMatch::new("rtc-ls7a")),
];

/// DriverOps::Data 即 Linux 的 struct foo_priv:probe 返回,框架持有,
/// remove 可变取回,remove 后 drop(其中 RAII guard 全部释放)。
pub struct Ls7aData {
    _publication: Publication<RtcDeviceImpl>, // 先发布者后资源,drop 反序
    _regs: IoMap,
}

pub struct Ls7aDriver;

impl DriverOps<PlatformBus> for Ls7aDriver {
    type Data = Ls7aData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Ls7aData, ProbeError> {
        let regs = dev.res().iomap(0)?;                       // devm_platform_ioremap_resource
        let device: RtcDeviceImpl = Arc::new(Ls7aRtc {
            base: regs.as_ptr() as usize,
        });
        let publication = dev.publish(&RTC, device, "rtc-ls7a")   // devm_rtc_device_register
            .map_err(map_probe_error)?;
        log::info!("rtc-ls7a: published at {:#x}", regs.region().base);
        Ok(Ls7aData { _publication: publication, _regs: regs })
    }
}

driver_base::declare_driver!(PlatformBus, Ls7aDriver, "rtc-ls7a", IDS);
```

**probe 的固定四步**:

1. 取资源:`dev.res().iomap(n)` / `mmio_by_name` / `irq(n)` / `clk(name)` / `alloc_coherent`(全部 devres 型 RAII)。
2. 实例化硬件层:把 `IoMap::as_ptr()` 基址(或区域句柄)交给硬件结构。
3. 发布:`dev.publish(&CLASS, Arc<impl ClassTrait>, "name")? -> Publication<T>`,存进 `Data`。
4. 错误用 `?` 传播;依赖未就绪(clk provider 未注册)返回 `ProbeError::Deferred`。

### 与 Linux 的对应速记

| Linux | v2 |
|-------|----|
| `of_device_id[]` | `static IDS: &[PlatformId]`(Of/Alias) |
| `module_platform_driver()` | `driver_base::declare_driver!(PlatformBus, …)` |
| `platform_set_drvdata` | `DriverOps::Data`(typed,无 downcast) |
| `devm_platform_ioremap_resource` | `dev.res().iomap(n)` |
| `devm_request_irq` | `dev.res().request_irq(spec, name, handler)` |
| `devm_clk_get` | `dev.res().clk(name)` |
| `devm_alloc_coherent` | `dev.res().alloc_coherent(len, align)` |
| `register_xxx / add_disk / misc_register` | `dev.publish(&class::CLASS, …)` |
| `dev_get_drvdata(remove)` | `remove(dev, data: &mut Self::Data)` |

## 硬件逻辑层标准形态

寄存器访问直接用 `IoMap`(volatile,无裸 `unsafe` 扩散);协议/状态机部分抽成**总线无关**的纯函数 + access trait,便于宿主机单测(ls7a 的 `ls7a_toy` 模块就是范例:时钟协议在 `ls7a_toy.rs`,每架构 CI 都跑其单测;`MmioToy` 只是生产环境 access 实现)。

```rust
// 硬件逻辑层只依赖:寄存器基址 + IoMap/裸 volatile + 中性错误类型
struct MmioToy { base: usize }

impl RtcToyAccess for MmioToy {
    fn read_toy_low(&mut self) -> u32 {
        // SAFETY: SYS_TOY_READ0 是映射区域内的 32 位寄存器偏移。
        unsafe { ((self.base + SYS_TOY_READ0) as *const u32).read_volatile() }
    }
}
```

- **不依赖** `Device`/`driver_base` 之外的框架;构造参数是基址或句柄。
- 每个寄存器偏移用 `const NAME: usize = 0x..;` 命名,注释锚定参考源文件(铁律 1)。
- 位段声明进 `data/bitfield-defs.json`,由 `ldp check-layout` 对参考树机械校验。

## 目录落点

| 内容 | 位置 |
|------|------|
| 类契约 trait + `class::` 静态 | `drivers/subsystems/<class>/`(host-free,可 mock) |
| 具体驱动(绑定层+硬件逻辑层) | `drivers/drv/<family>/src/<name>.rs` |
| 驱动资源桥(MMIO/IRQ/DMA/clk 后端) | `drivers/host`(DeviceServices over kirq/kdma/memspace/kclk) |
| 总线控制器(PCI 等) | `drivers/bus/<bus>/` |
| 组合根/注册调用 | `drivers/init`(initcall 层) |
| DT 解析 | `drivers/firmware/of` |

## 注册接线(v2 版"注册三连")

1. **id 表 + 驱动声明**:驱动文件内 `static IDS` + `declare_driver!(…)` —— 生成 `register()`。
2. **组合根调用**:`drivers/init` 的 initcall 序列里调用该驱动的 `register()`(驱动自身不碰全局注册表)。
3. **Kconfig/defconfig**:crate 由平台 Kconfig feature 选中(`.config` → `make defconfig`,见 build-workflow)。

## Checklist(每个新驱动过一遍)

- [ ] id 表完整(of compatible + 必要的 alias);`declare_driver!` 生成入口
- [ ] probe 全部资源经 `dev.res()`;RAII guard 存进 `Data`(顺序:Publication 在前)
- [ ] 硬件逻辑层无框架依赖、可单测;寄存器偏移全部命名 + 注释锚参考
- [ ] `unsafe` 有 SAFETY: 注释;位段进 `bitfield-defs.json`
- [ ] 缺失子系统进缺口报告(`ldp scan` 产草稿);`make clippy` 过
