<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 clk framework(rockchip_clk_register_branches)

| 字段 | 值 |
|------|----|
| ID | `req-clk-framework` |
| 关联缺口 | `clk-framework` (gaps.json) |
| 分类 | `multi-provider` |
| 状态 | `landed` |
| 来源 | `scan:../docs/ai/skills/linux-driver-port/data/references/linux/drivers/clk/rockchip/clk-rk3588.c` |
| 触发符号 | `rockchip_clk_register_branches` |
| 发现备注 | 命中符号: rockchip_clk_register_branches |

## 背景

**决策记录(2026-07,v2 移植)**:框架本体已随 v2 就位(core/kclk);本次 CRU 移植
补齐了框架增量(MuxFlags/DividerFlags HIWORD_MASK、DividerFlags READ_ONLY、
ClkFlags::NO_REPARENT、same-rate 短路、SET_RATE_PARENT 传播的父 notifier、
register_table 跨 provider 父名解析),驱动为 drivers/drv/clk/src/rk3588_cru/。
已知限制:CRU 软复位域无 reset framework 承接(gaps.json `reset`)。

v2:core/kclk 提供 prepare/enable/set_rate/notifier 全套语义;消费者 API 是 driver_base 的 Device::res().clk(name) -> ClkHandle(经 driver-host 桥接),缺 provider 时返回 ProbeError::Deferred。已有 provider:drv/clk 的 fixed-clock/fixed-factor。RK3588 CRU(PLL/门控/软复位)provider 未移植 —— USB/PCI 移植的前置。

Linux 侧相关符号:`clk_get`, `clk_prepare_enable`, `clk_disable_unprepare`, `clk_set_rate`, `clk_get_rate`, `of_clk_add_hw_provider`, `of_clk_add_provider`, `clk_hw_register`, `clk_register_composite`, `clk_register_divider`, `clk_register_mux`, `clk_register_gate`, `clk_register_frac_branch`, `rockchip_clk_register_branches`, `rockchip_clk_register_plll`。

## 分类依据(provider 判据)

该缺口被归为 provider 类。在确认前,请逐条核对判据——**五条全 pass 才值得抽 provider;任一 fail → 改归类(改 gaps.json 的 infra_type)。**

| 判据 | 问题 | 本 gap 评估 |
|------|------|------------|
| **物理基础设施** | 是 SoC 物理性服务设施(供时钟/供电/引脚/映射),物理存在? | ✓ pass |
| **多生产者** | 该能力有 ≥2 个独立来源,各管自己的命名空间? | ✓ pass |
| **消费者泛化** | 消费者写「我要这个能力」,不关心是谁提供的(按名字/id 取)? | ✓ pass |
| **契约跨内核稳定** | 能力语义在 Linux/RTOS/x-kernel 高度一致? | ✓ pass |
| **收益 > 成本** | 至少 N 个实现 × M 个消费者(provider trait 有足够复用面)? | ✓ pass |

> 已记录论证:clk-subsystem.md 已论证 Stage 1(晶振物理存在、CRU/SCMI/fixed-clock 多 provider、dev.res().clk 按名取、get_rate/enable 语义通用、数十 provider×数十驱动)。Stage 2 三坑也已于 2025-08 修复:name 键(去 phandle)、trait 定全(含 enable/disable/set_rate 默认)、ClkRegistry 可注入(去 kspin);Cargo.toml 已去 of/kspin 依赖(见 redline)。现为本目录唯一 Stage1+Stage2 双全的 gap。

判据参考(完整边界表 + A/B/concrete 三分见 `data/provider-boundary.json`):
- A 类(provider 能力):横向「提供什么能力」,多 provider × 多消费者,OS-agnostic 目标。
- B 类(device class 契约):纵向「是什么设备」,每内核重写数据面,**不是 provider**。
- concrete(具体驱动):寄存器层复用 + 数据面重写,**不是 provider**。

## 实现质量清单(cross-kernel 坑,Stage 2)

provider 已确认该抽。**实现时逐条对照,避免「名义 OS-agnostic 实际被绑死」**:

| 坑 | 错误做法 | 正确做法 | 机械查? |
|----|---------|---------|--------|
| **注册键用了固件描述特定物** | provider key = DT phandle(phandle 是 DT 特有,ACPI/PCI 平台没有) | provider key = OS 中性 name(DT 路径=节点名去 @unit;ACPI/PCI 各自翻译) | 机械查:中性 crate 不得依赖 of/DT crate |
| **trait 只定当前 Tier 的方法,未来 Tier 注释占位** | 只定 get_rate,enable/disable/set_rate 写成注释 | 一次性定义完整方法集,当前 Tier 给默认实现(enable 默认 Ok、set_rate 默认 Unsupported) | 人工:Tier N 实现不完整时,trait 仍应含所有 Tier 的方法签名 + 默认实现 |
| **注册表全局静态 + 不可注入** | static REGISTRY 线性扫描,测试靠 reset_registry 绕 | 注册表做成可注入 struct(ClkRegistry::new()),进程全局只是默认;用 core-only 自旋锁去 kspin 依赖 | 机械查:中性 crate 不得依赖 k* crate(kspin 等);人工:registry 有 ::new() 可自建 |

> 🔴 红线(机械可查):声称 OS-agnostic 的 provider crate,其 Cargo.toml 不得依赖任何 k* crate 或 of(DT) crate
>   `ldp check-provider-crate <Cargo.toml>`

三坑从 clk 实证归纳(详见 `data/provider-boundary.json` 的 impl_pitfalls)。

## 建议实现路径

按 clk 模式(provider 模式,v2 参考 core/kclk):
1. 定义中性核心 crate(如 core/kclk):Provider ops + lookup(handle/parent/拓扑)。
2. consumer API 走 Device::res() 形式:`dev.res().clk(name)`,总线把 DT(`clocks`/`resets`/`phys`/`*-supply`)预解析进 ResourceMap,驱动不碰 DT。
3. driver-host 扮演 DeviceServices 桥,把资源请求转给已注册的 provider。
4. DT 解析在 drivers/firmware/of + drivers/base;平台 provider 由组合根 (drivers/init)在 initcall 阶段注册。
5. 参考:`core/kclk`(核心)+ `drivers/drv/clk`(fixed provider)+ `drivers/host`(桥)。

> 这是工作量最大的一类(约 1-2k 行),因为它要建一整套 provider/consumer 框架。

## 验收标准

- [ ] `ldp check-data && ldp check-bitfields && ldp golden` 全过
- [ ] gaps.json 对应条目 `status` 改 `landed`
- [ ] Provider trait + 注册表实现
- [ ] consumer API(`devm_*`)可用,至少一个驱动验证
- [ ] DT 解析在总线层生效(驱动不碰 DT)

---

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-clk-framework` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
