# 下一步工作计划(2026-10-26 起)

> 目标函数:(a) 最短路径拿到 **SD 根文件系统启动闭环**(已在最后一公里);
> (b) 按 `difficulty-assessment.md` 波次推进 RK3588 六方向,波次内顺序按
> "下一个驱动需要什么"拉取基础件,不全量铺框架;(c) 约束:PR 1081 未合入,
> 新代码落在 `driver-test` 线(基线 db76e8a94),避免触碰 `drivers/base`。

## T0 — SD 闭环(本周尺度,最高优先)

| # | 事项 | 说明 | 验收 |
|---|---|---|---|
| T0.1 | **board-fact clock provider** 解除 defer | 新增 `drv/clk` 的板级时钟驱动:按 Kconfig 列表 `(phandle, id, rate)` 发布 `ClkProvider`(SD 只需 0x0e:0x17/0x09,CRU 0x02 的 drive/sample 发布 0 Hz 直通即可)。不改 `drivers/base`(框架依赖机制保持不动,与 PR 1081 零冲突) | debug.log 不再出现 clk defer,dwmmc probe 到位 |
| T0.2 | 板级复测 `d1ffbee44` 解楔 | 512B/1024B 数据相位;不过则 `MMC_MAX_CARD_HZ` 降速 bisect;仍不过则切 **SDMA 单描述符**第三路线 | LBA0 阶梯 1/2/4/8 块全过 |
| T0.3 | 启动闭环 | mmcblk0pN → root mount → SD 起用户态;成功日志归档,`drv/mmc/docs/design.md` 补数据路径终态(PIO vs SDMA) | 从 SD 挂根进 shell |
| T0.4 | SCMI 线关闭 | `scmi_clock.rs` 保留为负结果资产(bl31 不服务主线 SCMI clock,结论已入代码注释),不再投时间;`CLK_SCMI` 默认关 | Kconfig 描述补"本板不可用"结论 |

## T1 — eMMC(sdhci-dwcmshc),存储稳定化

- 前置一:**最小 CRU provider** —— `clk-rk3588` 120 KB 全表裁成"所需 ids
  白名单"(eMMC 五条 clock + 五条 reset,DT 已给 assigned-clock-rates),
  同 T0.1 的 board-fact 模式;**最小 reset 框架**顺势提前(sdhci 节点 5 条
  reset 是第一个真实消费者)。
- 前置二:eMMC 状态机(CMD1/EXT_CSD/HS400ES strobe)进 `subsystems/mmc`
  (契约/卡总线/blk 全复用,scope cut 里已预留)。
- 验收:root fs 迁 eMMC,SD 只作交换介质。

## T2 — 按需基础件(不单独成波,随 T3–T5 拉取)

| 件 | 第一个真实消费者 | 备注 |
|---|---|---|
| GPIO(+pinctrl 最小面) | SD cd-gpios(现走轮询)、fusb302 IRQ、gt9271 IRQ、PCIe perst | U-Boot 已配的 pinmux 先当事实用(SD 即此形态) |
| I2C 子系统 | gt9271 触摸屏(板上已 `okay`)、hym8563 RTC | RTC 是现成 rtc 类的顺手第二例 |
| SPI | RK806 PMIC | regulator 只在 UHS SDR104(vqmmc 1.8V)与 PCIe vpcie3v3 时才变刚需,先挂账 |
| power domain 最小"确保 on" | pd_usb/pd_vop | U-Boot 预配可顶,SCMI 不可用则走 GRF 直写,随第一消费者定 |

## T3 — USB2 host(usbcore + ehci/ohci)

- 大头是 **usbcore 中件层**(URB 等价物、hub 轮询、枚举状态机)——先出
  设计文档(host-free 契约 + mock),再落 ehci/ohci 本体(小)。
- u2phy2/3 host 口不需要 TCPM/Type-C,PHY 框架按 inno-usb2 最小面起步。
- 验收:Type-A 键盘 + U 盘(mmcblk 之外的第二个块来源)。

## T4 — PCIe(pcie-dw-rockchip + snps-pcie3 + combphy)

- 一次解锁:网×2(r8169 裁剪版,net 类走 legacy 过渡门或迁类)、NVMe、
  (远期 WiFi)。MSI/os-api 就绪,PCI 枚举旧侧已有,RC 窗口自建。
- 依赖 T2 的 reset 最小框架(perst GPIO + PHY reset)。
- 验收:r8169 双网口 DHCP + NVMe 块设备。

## T5 — 显示最小化 + 触摸输入(小车场景首验)

- VOP2 固定模式扫出 + HDMI-QP + hdptxphy(裁剪;板上 DT 目前 HDMI
  `disabled`,验证前先开节点)。
- gt9271 触摸屏(T2 I2C 之后)进 input 类;整机最小验收集 =
  显示 + 触摸 + USB 外设 + 网。

## 治理项(并行,不占驱动人力)

- **G1** PR 1081 合入后 `driver-test` 线 rebase 到三 crate 形态:先做一次
  API 映射评估(`driver_base::X` → `model/class/os-api` 落点),41 个 commit
  里的 `drivers/base` 触点很少(仅依赖自动 defer/发布面),预计 rebase 面
  可控。
- **G2** 两条 v2 栈(v2-refactor 22c vs PR 1081 93c)合流策略——PR 评审
  收敛后第一件事。
- **G3** 已验证模式反哺 `linux-porting-map`:board-fact 资源路线、provider
  对偶(publish/provide)、XferSignal 双半完成设计、卡总线(设备挂设备)。

## 附录:USB 与 PCIe 的子系统依赖矩阵(板上 DT 实证,2026-10-26)

> 依据:`orangepi-5-plus-m.dts` 中 USB/PCIe 节点的 clocks/resets/phys/
> power-domains/msi-map/phandle 解析。所有时钟都在 CRU(phandle 0x02)
> ——不是 SCMI;即 T0.1 的 board-fact clock provider 模式可直接复用。

### 共同必需(USB + PCIe)

| 子系统 | 板上事实 | 最小形态 | 备注 |
|---|---|---|---|
| CRU 时钟 provider | USB 3+4 条/控制器、PCIe 6 条/RC、PHY pclk,全是 CRU ids | T0.1 同款 board-fact 白名单(也可解框架自动 defer) | 无它设备在枚举期就 park |
| reset 框架(最小) | dwc3 1 条/控制器、pcie30phy 1 条、eMMC 5 条 | CRU reset 线 provider(SOFTRST 寄存器面) | eMMC 是第一个消费者,USB/PCIe 复用 |
| PHY 框架(最小) | USB: inno-usb2(syscon GRF 内)+usbdp;PCIe: pcie3-phy(需 pipe-grf+phy-grf 两窗)、combphy×2 | provider 模式照抄 clk;GRF 共享窗直接 `iomap`,不做通用 syscon | USB2 口只需 inno-usb2 |
| GPIO(最小) | EHCI VBUS 使能、PCIe perst/vpcie3v3、SD cd | GPIO bank 驱动(gpio-rockchip 21 KB)直写;pinmux 用 U-Boot 事实 | 不做 pinctrl 全量框架 |
| power domain("确保 on") | PD_USB(0x1f)/PD_PCIE(0x22),挂在 power-management@fd8d8000 | U-Boot 预配当事实或 PMU GRF 直写,随第一消费者定 | |

### 仅 USB 需要

| 子系统 | 说明 |
|---|---|
| **usbcore 中件层** | URB 等价物、hub 轮询、设备/端点枚举状态机——真大头,先设计后实现 |
| EHCI/OHCI host 驱动 | 本体小(Type-A 口先行,不碰 dwc3) |
| VBUS regulator | 板上是 regulator-fixed+GPIO;常开可先当事实 |
| (后置)TCPM/extcon/role-switch | 仅 Type-C 口(fc000000+fusb302,要 I2C);dr_mode=host 可绕过 |

### 仅 PCIe 需要

| 子系统 | 说明 |
|---|---|
| DWC RC 驱动 | pcie-dw-rockchip(28 KB):atu/viewport 出站窗、INTx demux 子节点、link 训练 |
| INTx 中断 | 每 RC 的 legacy-interrupt-controller 子节点→GIC SPI;kirq 现成 |
| **GICv3 ITS**(缺!) | msi-map 指向 ITS@fe660000;x-kernel gicv3 目前无 ITS/LPI。设备先跑 INTx bring-up(r8169/NVMe 均可),MSI/MSI-X 是后续独立任务 |
| PCI 枚举对接 | 旧侧 bus/pci 已有(ECAM/发现/MSI-X 词汇);换成 RC 自建窗口 |

### 明确不需要(先排除)

I2C/SPI(fusb302 之外无人需要)、regulator 全框架(GPIO 直写顶)、
pinctrl 全框架(U-Boot 事实)、IOMMU/smmu-v3(bypass)、TCPM(host 模式)、
SCMI(板上 bl31 不服务,negative result 已归档)。

### 建议铺底顺序

```
1. CRU board-fact provider(T0.1 直接复用) ──► 解 USB/PCIe/eMMC 三线时钟 defer
2. 最小 reset + GPIO 直写                     ──► 复位链 + 使能链全通
3. 最小 PHY 框架(inno-usb2 先行)             ──► EHCI/Type-A 可点亮
4. 二线并行:EHCI+usbcore设计 ‖ DWC RC(INTx 模式)
5. 后置:GICv3 ITS(MSI)、dwc3/xHCI、Type-C
```

## 优先序与并行建议

```
T0(串行,一人) ──► T1(串行,同一人:MMC 上下文最热)
T3 usbcore 设计文档 ─┐
                     ├─ T0 完成后二线并行:一线 T1(eMMC),一线 T3(USB)
T4 框架小件(reset/phy)┘  之后 T4,最后 T5(依赖 T2 的 I2C/GPIO)
```
