<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 忙等延时(udelay/mdelay/ndelay)(udelay)

| 字段 | 值 |
|------|----|
| ID | `req-busy-wait` |
| 关联缺口 | `busy-wait` (gaps.json) |
| 分类 | `framework` |
| 状态 | `confirmed`(2026-09-06:仓库已有原语,驱动侧直接使用,无需新框架) |

## 决策

PLL 锁等待/轮询使用现有原语 `khal::time::busy_wait(TimeSpan::from_micros(n))`(arch/khal/src/time/mod.rs),
不新建框架。RK3588 PLL 锁上限 1000µs,采用 deadline 轮询(每次读 PLLCON6 后短忙等),超时报错走恢复路径。
| 来源 | `scan:docs/ai/skills/linux-driver-port/data/references/linux/drivers/clk/rockchip` |
| 触发符号 | `udelay` |
| 发现备注 | 命中符号: udelay |

## 背景

无 busy-wait helper。

Linux 侧相关符号:`udelay`, `mdelay`, `ndelay`。

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

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-busy-wait` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
