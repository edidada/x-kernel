# Demo 走查:Linux rtc-loongson → x-kernel LS7A RTC

这是 skill 的完整实例。对照项目**真实实现**:
- 驱动(绑定+硬件逻辑):[drivers/drv/rtc/src/ls7a.rs](../../../../../drivers/drv/rtc/src/ls7a.rs)
- 协议/状态机(总线无关,可单测):[drivers/drv/rtc/src/ls7a_toy.rs](../../../../../drivers/drv/rtc/src/ls7a_toy.rs)
- 类契约:`drivers/subsystems/rtc`(RtcDevice trait + `class::RTC`)

> **读这个 demo 要带走的头号收获**:x-kernel(v2)的驱动面就是
> `DriverOps::probe` 里的 `dev.res()` 资源获取 + `dev.publish(&CLASS, …)` 发布,
> 外加文件末尾一行 `declare_driver!`。Linux 的 devm 家族、of_match、drvdata
> 全部有直接对应物;对应不上的才是缺口(照报)。

目标硬件:Loongson LS7A RTC(`compatible = "loongson,ls7a-rtc"`,另覆盖 QEMU
LoongArch virt 的 DT 节点)。Linux 参考:`drivers/rtc/rtc-loongson.c`(已 vendor
进参考树 `data/references/linux/drivers/rtc/rtc-loongson.c`)。

---

## Phase 0 — 采集

| 项 | 值 |
|----|----|
| Linux 源 | `drivers/rtc/rtc-loongson.c`(linux-7.1.8,已 vendor) |
| compatible | `loongson,ls7a-rtc` |
| 设备类 | RTC(`subsys_rtc::RtcDevice` 契约) |
| 落点 | `drivers/drv/rtc/src/ls7a.rs`(绑定+MMIO)+ `ls7a_toy.rs`(协议) |
| 项目已存在? | **是** → 本 demo 是"对照/说明"模式,真实代码即标准答案 |

## Phase 1 — 分析(Linux 画像)

1. **probe 流程**(`loongson_rtc_probe`):
   - `platform_get_resource`(MMIO)→ `devm_platform_ioremap_resource`
   - 读 `RTC_CTRL_REG` 判断 TOY 使能状态
   - `devm_rtc_allocate_device` + `rtc_register_device`(向 RTC class 注册)
   - (部分变体)acpi/OF 数据选择 config
2. **寄存器协议**:TOY_READ0/READ1 双字读出(秒/分/时/日/月/年字段),年边界一致性
   检查(高字与低字配合);`TOY_*` GENMASK 位段是**硬件事实**,原样翻译。
3. **私有数据**:`struct loongson_rtc_priv`(regmap 基址、rtcdev 指针)。
4. **依赖子系统**:MMIO ✅ / RTC class ✅;无 clk/irq/phy 依赖(ls7a 变体)。

## Phase 2 — 映射

| Linux | x-kernel(v2) | 依据 |
|-------|---------------|------|
| `of_match_table = loongson_rtc_of_match[]` | `static IDS: &[PlatformId]`(Of + Alias) | drivers/base/src/bus/platform_bus.rs:178 |
| `devm_platform_ioremap_resource` | `dev.res().iomap(0)? -> IoMap` | drivers/base/src/device.rs:327 |
| `readl/writel`(TOY_READ0/1、CTRL) | `IoMap::read32/write32` | drivers/base/src/devres/iomap.rs:94 |
| `devm_rtc_device_register` | `dev.publish(&subsys_rtc::class::RTC, Arc<impl RtcDevice>, "rtc-ls7a")` | drivers/subsystems/rtc/src/class.rs:24 |
| `platform_set_drvdata(priv)` | `DriverOps::Data`(Ls7aData,框架持有) | drivers/base/src/driver.rs:15 |
| `module_platform_driver()` | `driver_base::declare_driver!(PlatformBus, Ls7aDriver, "rtc-ls7a", IDS)` | drivers/base/src/driver.rs:65 |
| `dev_err/dev_info` | `log::error!/info!`(拼 `dev.info().path()`) | api-mapping §9 |

**没有缺口** —— 这是一个"API 级映射全覆盖"的驱动,直接 1:1 落地。
golden 基线(`data/golden/ls7a-rtc.json`)断言上表全部映射存在。

## Phase 3 — 生成(对照真实代码)

绑定+发布(`ls7a.rs` 节选,完整见源文件):

```rust
static IDS: &[PlatformId] = &[
    PlatformId::Of(OfMatch::compatible("loongson,ls7a-rtc")),
    PlatformId::Alias(AliasMatch::new("rtc-ls7a")),
];

pub struct Ls7aData {
    _publication: Publication<RtcDeviceImpl>, // 先发布者后资源,drop 反序
    _regs: IoMap,
}

impl DriverOps<PlatformBus> for Ls7aDriver {
    type Data = Ls7aData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Ls7aData, ProbeError> {
        let regs = dev.res().iomap(0)?;
        let device: RtcDeviceImpl = Arc::new(Ls7aRtc { base: regs.as_ptr() as usize });
        let publication = dev.publish(&RTC, device, "rtc-ls7a")?;
        Ok(Ls7aData { _publication: publication, _regs: regs })
    }
}

driver_base::declare_driver!(PlatformBus, Ls7aDriver, "rtc-ls7a", IDS);
```

要点:

- **协议与访问分离**:`read_rtc_from(&mut impl RtcToyAccess)` 是纯函数(年边界
  一致性检查在里面),宿主机 CI 直接单测;`MmioToy` 只是生产环境 access 实现。
  这是"硬件逻辑层可单测"的标准做法。
- **RAII**:`Publication` 与 `IoMap` 都存进 `Data`;remove 后框架按逆序 drop
  (先释放映射,再反发布 —— 字段顺序即 drop 顺序)。
- **无裸 unsafe 扩散**:volatile 访问收敛在 access impl 的三小段,各带 SAFETY:。

## Phase 4 — 缺口报告

```
ls7a-rtc 移植缺口报告

- Linux 源: drivers/rtc/rtc-loongson.c(linux-7.1.8,已 vendor)
- compatible: loongson,ls7a-rtc
- 目标类: subsys_rtc(RtcDevice)
- 落点: drivers/drv/rtc/src/ls7a.rs(+ls7a_toy.rs 协议)
- 移植结论: 可完整工作(read_time;set_time/alarm 按 ls7a 硬件能力裁剪)

依赖子系统: MMIO✅ RTC-class✅  clk(不依赖) irq(不依赖)

逐条缺口: 无 —— API 级映射全覆盖。
限制: ls7a 变体未实现 alarm(TOY_MATCH0-2)与 set_time 写路径,与
Linux 同名变体行为对齐,属能力裁剪而非缺口。
```

## Phase 5 — 集成

- crate `drv-rtc` 已在平台 Kconfig;组合根(`drivers/init`)调用 `register()`。
- 校验:`make build && make clippy`;启动后经 RTC 类 hctosys 验证读数
  (`subsys_rtc::hctosys()`)。

## 教训总结

1. **对应不上的才叫缺口**。devm/of_match/drvdata/publish 这些骨架在 v2 全有
   直接对应,不要把它们报成缺口;真正缺的是 phy/reset/gpio/regulator/msi/
   syscon/usb/i2c 这类子系统(见 `gaps.md`)。
2. **协议层抽 access trait**,驱动主体可宿主机单测 —— 这是 v2 双层结构的红利,
   移植复杂控制器(USB/PCI)时同样适用(xHCI ring/DWC3 协议都该这样拆)。
3. **drop 顺序即字段顺序**:Publication 放第一,资源 guard 放后面,反发布永远
   最后发生。
