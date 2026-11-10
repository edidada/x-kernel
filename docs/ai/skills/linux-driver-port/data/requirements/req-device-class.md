<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 设备子系统无 class(i2c/spi/watchdog/regulator/usb)(usb_hcd)

| 字段 | 值 |
|------|----|
| ID | `req-device-class` |
| 关联缺口 | `device-class` (gaps.json) |
| 分类 | `subsystem-class` |
| 状态 | `rejected` |
| 来源 | `scan:../docs/ai/skills/linux-driver-port/data/references/linux/drivers/usb/host/ehci-hcd.c` |
| 触发符号 | `usb_hcd` |
| 发现备注 | 命中符号: usb_hcd |

## 背景

v2 已有 class:block/net/char/rtc/input/hwrng/display/vsock(drivers/subsystems/<class>,publish 走 Device::publish(&CLASS, impl, name))。仍缺:i2c、spi、watchdog、regulator、USB 主机栈的 class/总线。

Linux 侧相关符号:`i2c_register_driver`, `spi_register_driver`, `watchdog_register_device`, `regulator_get`, `usb_register`, `usb_hcd`。

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

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-device-class` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
