<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 USB 主机栈(usb core + HCD)(ehci_setup)

| 字段 | 值 |
|------|----|
| ID | `req-usb-stack` |
| 关联缺口 | `usb-stack` (gaps.json) |
| 分类 | `subsystem-class` |
| 状态 | `proposed` |
| 来源 | `scan:../docs/ai/skills/linux-driver-port/data/references/linux/drivers/usb/host/ehci-hcd.c` |
| 触发符号 | `ehci_setup` |
| 发现备注 | 命中符号: ehci_setup |

## 背景

**决策(2026-07)**:首版只做 host 角色:usb core 最小面(hub 枚举/设备地址/描述符解析)+ EHCI(qh/qtd ring,coherent 内存)与 OHCI;xHCI/dwc3(USB3)后排。RK3588 EHCI/OHCI 对是通用硬件(rockchip,rk3588-ehci 兼容 generic-ehci),usb2phy 是前置(req-phy 共用)。

无 usb core/hub/设备模型,无 EHCI/OHCI/xHCI HCD,无 DWC3。RK3588:2×dwc3(OTG)+1×dwc3(host)+4×EHCI/OHCI 对。首版切片建议 EHCI/OHCI(通用硬件逻辑)+ usb2phy。

Linux 侧相关符号:`usb_hcd_probe`, `ehci_setup`, `ohci_init`, `xhci_gen_setup`, `dwc3_probe`, `usb_hub_init`, `usb_register`。

## 建议实现路径

新设备类(v2:class 契约不需要新 DeviceKind):
1. 新建 drivers/subsystems/<class> crate:定义该类的设备 trait(如 `I2cController`),含总线操作,方法返回 kerrno::KResult。
2. 加 `class::<T>` 静态 + `Device::publish(&CLASS, Arc<impl>, name) -> Publication`(参考 subsys_rtc,最简)。
3. 子设备(总线上的从设备)如何挂载是设计重点(如 i2c controller 要能注册子 i2c 设备)。
4. 组合根注册:驱动文件 `driver_base::declare_driver!(...)` + drivers/init 调 register()。
5. 参考:`subsys_rtc`/`subsys_char` 现有模式 + `drivers/drv/rtc` 消费者。
	**这是 RK3588 i2c/spi 移植最常见的 blocker。**

## 验收标准

- [ ] `ldp check-data && ldp check-bitfields && ldp golden` 全过
- [ ] gaps.json 对应条目 `status` 改 `landed`
- [ ] DeviceKind 新变体已加
- [ ] publish_* 宏可用
- [ ] 至少一个 controller 驱动 + 子设备挂载验证

---

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-usb-stack` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
