<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 regulator(devm_regulator_get)

| 字段 | 值 |
|------|----|
| ID | `req-regulator` |
| 关联缺口 | `regulator` (gaps.json) |
| 分类 | `multi-provider` |
| 状态 | `rejected` |
| 来源 | `scan:../docs/ai/skills/linux-driver-port/data/references/linux/drivers/pci/controller/dwc/pcie-dw-rockchip.c` |
| 触发符号 | `devm_regulator_get` |
| 发现备注 | 命中符号: devm_regulator_get |

## 背景

**决策(2026-07,rejected-for-now)**:板级供电为固定轨(vcc3v3-pcie2x1l0 由 gpio-fixed-regulator 控制,本切片假定固件已使能,日志标注假设)。出现可开关供电的真实消费者(vmmc 类)再立项。

无 regulator 框架。

Linux 侧相关符号:`regulator_enable`, `devm_regulator_get`。

## 影响范围

根据 gaps.json 的级联缺口表,该子系统阻塞以下设备类:
- USB3 (dwc3)
- GPU/VPU/codec

## 分类依据(provider 判据)

该缺口被归为 provider 类。在确认前,请逐条核对判据——**五条全 pass 才值得抽 provider;任一 fail → 改归类(改 gaps.json 的 infra_type)。**

| 判据 | 问题 | 本 gap 评估 |
|------|------|------------|
| **物理基础设施** | 是 SoC 物理性服务设施(供时钟/供电/引脚/映射),物理存在? | ? 待评估 |
| **多生产者** | 该能力有 ≥2 个独立来源,各管自己的命名空间? | ✓ pass |
| **消费者泛化** | 消费者写「我要这个能力」,不关心是谁提供的(按名字/id 取)? | ✓ pass |
| **契约跨内核稳定** | 能力语义在 Linux/RTOS/x-kernel 高度一致? | ✓ pass |
| **收益 > 成本** | 至少 N 个实现 × M 个消费者(provider trait 有足够复用面)? | ? 待评估 |

> 已记录论证:RK3588 板级供电多由固定 on 的 LDO;可开关 supply 出现(vmmc 类)再立项。单 provider 语义存疑(多 PMIC),Stage 1 待评估。

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

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-regulator` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
