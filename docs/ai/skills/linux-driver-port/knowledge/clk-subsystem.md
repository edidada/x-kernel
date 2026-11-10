# clk 子系统(v2):core/kclk 分层 + 消费者/提供者用法

移植 RK3588 外设(MMC/I2C/UART/SPI/USB/PCI)几乎必撞 clk。本文是撞到 `clk_get` / `clk_get_rate` / `clk_prepare_enable` / `clk_set_rate` 时的对应指南。

## v2 分层(三层)

```
┌──────────────────────────────────────────────────────────────┐
│ 消费者(驱动)                                                    │
│   dev.res().clk("name") -> ClkHandle                          │
│   ClkHandle::{get_rate, prepare_enable, set_rate, …}           │
│   缺 provider 时 probe 得到 ProbeError::Deferred,泵自动重试      │
└──────────────────────────┬───────────────────────────────────┘
                           │ DeviceServices 桥
┌──────────────────────────▼───────────────────────────────────┐
│ driver-host(drivers/host/src/clk.rs)                          │
│   ClkRegistry:register_provider(&'static dyn ClkLookup)        │
│   按 ClkSpec{provider(phandle), index(clock-specifier)} 解析    │
└──────────────────────────┬───────────────────────────────────┘
                           │ ClkProvider 门(dev.env().provide::<ClkProvider>)
┌──────────────────────────▼───────────────────────────────────┐
│ core/kclk(中性核心,无驱动框架依赖)                               │
│   ClkCore / ClkBuilder / FixedRate / Divider / Mux / Gate …     │
│   prepare/enable/set_rate/notifier 全套语义                      │
└──────────────────────────────────────────────────────────────┘
```

- **注册时机**:provider 驱动 probe 里发布(如 `drivers/drv/clk` 的 fixed-clock);组合根按 initcall 顺序枚举,consumer Deferred 由 probe 泵唤醒。
- **boot-static**:clock controller 的 lookup 常驻 `'static`(对应 Linux `of_clk_add_hw_provider`),不随 probe 失败回滚。

## 消费者怎么用

```rust
// Linux: clk = devm_clk_get(dev, "ciu"); clk_prepare_enable(clk); rate = clk_get_rate(clk);
let clk = dev.res().clk("ciu")?;        // ClkHandle;缺 provider -> ProbeError::Deferred
clk.prepare_enable()?;
let rate = clk.get_rate();              // Rate 类型
```

- set_rate/range/parent:`ClkHandle::{set_rate, set_rate_range, round_rate, set_parent}`(core/kclk 语义与 Linux CCF 对齐:传播、notifier、repaint 由核心处理)。
- **别想当然读 DT `clock-frequency`**:Rockchip MMC 节点通常只有 `max-frequency`(卡时钟上限/caps),不是 CIU 输入速率,不能当分频分母。

## 提供者怎么写(provider 驱动模板)

`drivers/drv/clk/src/fixed_clock.rs` 是 **provider 驱动的黄金模板**:一个文件,注册即数据:

```rust
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("fixed-clock"))];

impl DriverOps<PlatformBus> for FixedClockDriver {
    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Self::Data, ProbeError> {
        let fw = dev.fw();
        let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let frequency = firmware.clock_frequency.ok_or(...)?;
        let phandle = firmware.phandle.ok_or(...)?;

        let core = ClkBuilder::new(name, FixedRate::new(frequency as u64).into_ops())
            .build().map_err(...)?;
        let lookup: &'static FixedClockLookup = Box::leak(Box::new(
            FixedClockLookup { provider_id: phandle, core: core.clone() }));
        // 发布放最后:注册唤醒 park 在该 phandle 上的 Deferred 消费者
        dev.env().provide::<driver_host::ClkProvider>(phandle, lookup)?;
        Ok(Data { _core: core })
    }
}
```

RK3588 CRU 即按此模式落成 `drivers/drv/clk` 的 cru provider(PLL/mux/divider/gate + 软复位),DT 索引用 BSP 头(`bsp-opi5plus` 参考树),寄存器/时序以 `linux` 参考树的 `drivers/clk/rockchip/clk-rk3588.c` 为准。

## SCMI 消息头位段坑(历史教训,勿重蹈)

SCMI 时钟(`CLOCK_RATE_GET`)消息头是 32 位:`message[0:8] / message_type[8:10] / protocol[10:18] / token[18:28]`。曾因凭记忆把 `message_id`/`token` 记反,把"位段 bug"误诊为"固件 stub"。纪律:位段声明进 `data/bitfield-defs.json`,`ldp check-layout` 从钉死参考树机械导出真值 diff claim;验证用非零字段。

## 已知现状(2026-07)

- core/kclk:落地(prepare/enable/set_rate/notifier/parent 传播)。
- 消费端:落地(`Device::res().clk` + Deferred)。
- provider:fixed-clock / fixed-factor 已有;**RK3588 CRU 未移植**(见 gaps.json `clk-framework`,partial)。
- 软复位:CRU 的 reset 控制器域无 reset framework 承接(见 gaps.json `reset`)。
