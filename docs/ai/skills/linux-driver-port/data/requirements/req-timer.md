<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 定时器(timer_setup/mod_timer/hrtimer)(timer_setup)

| 字段 | 值 |
|------|----|
| ID | `req-timer` |
| 关联缺口 | `timer` (gaps.json) |
| 分类 | `framework` |
| 状态 | `proposed` |
| 来源 | `scan:../docs/ai/skills/linux-driver-port/data/references/linux/drivers/usb/host/ehci-hcd.c` |
| 触发符号 | `timer_setup` |
| 发现备注 | 命中符号: timer_setup, mod_timer, hrtimer |

## 背景

task/ktimer-core:TimerSet + Timer(new/rearm/try_cancel/cancel_sync),回调在定时器上下文;delayed-work 走 kworkng::DelayedWorkHandle。hrtimer 精度等级另报。

Linux 侧相关符号:`timer_setup`, `mod_timer`, `hrtimer`。

## 建议实现路径

关键子框架(非 provider 模式,需具体框架设计):
1. 识别这个框架的核心抽象(如 regmap=寄存器 RMW,workqueue=延后执行,timer=定时回调)。
2. 设计 trait + 实现,通常基于现有原语(`Io`/`ktask`/`WaitQueue`)。
3. 不需要 provider 注册表或新 DeviceKind。
4. 工作量因框架而异,先评估是否值得建框架 vs. 驱动侧就地替代(见 gaps.json 的 workaround)。

## 验收标准

- [ ] `ldp check-data && ldp check-bitfields && ldp golden` 全过
- [ ] gaps.json 对应条目 `status` 改 `landed`
- [ ] 框架核心抽象可用
- [ ] 至少一个驱动用新框架替代 workaround

---

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-timer` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
