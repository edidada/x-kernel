<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 clk framework(clk_disable_unprepare)

| 字段 | 值 |
|------|----|
| ID | `req-clk-framework` |
| 关联缺口 | `clk-framework` (gaps.json) |
| 分类 | `multi-provider` |
| 状态 | `confirmed`(2026-09-06,随 RK3588 CRU 移植决策) |
| 来源 | `scan:docs/ai/skills/linux-driver-port/data/references/linux/drivers/clk/rockchip` |
| 触发符号 | `clk_disable_unprepare` |
| 发现备注 | 命中符号: clk_disable_unprepare, clk_divider_ops, clk_fractional_divider_ops, clk_gate_ops, clk_get, clk_get_rate, clk_hw_register, clk_hw_register_composite, clk_mux_ops, clk_prepare_enable, clk_register_divider, clk_register_gate, clk_register_mux, clk_set_rate, of_clk_add_provider, of_clk_src_onecell_get |

## 背景

core/kclk 通用时钟框架已落地主线(core/kclk:两阶段 prepare/enable、全局 provider 注册表(ProviderId=DT phandle)、消费者 ClkHandle、ClkBranch 静态表事务注册、PRE/POST rate notifier、clk_summary/clk_disable_unused)。device-res 有 clk 消费/提供者契约,kdriver 平台总线按 provider-first 顺序 probe,clk-fixed-clock 驱动已接 DT fixed-clock。RK3588 CRU 是首个真实 SoC 控制器驱动。

Linux 侧相关符号:`clk_disable_unprepare`, `clk_divider_ops`, `clk_fractional_divider_ops`, `clk_gate_ops`, `clk_get`, `clk_get_rate`, `clk_hw_register`, `clk_hw_register_composite`, `clk_mux_ops`, `clk_prepare_enable`, `clk_register_composite`, `clk_register_divider`, `clk_register_fixed_rate`, `clk_register_gate`, `clk_register_mux`, `clk_set_rate`, `devm_clk_hw_register`, `of_clk_add_provider`, `of_clk_src_onecell_get`。

## 分类依据(provider 判据)

该缺口被归为 provider 类。在确认前,请逐条核对判据——**五条全 pass 才值得抽 provider;任一 fail → 改归类(改 gaps.json 的 infra_type)。**

| 判据 | 问题 | 本 gap 评估 |
|------|------|------------|
| **物理基础设施** | 是 SoC 物理性服务设施(供时钟/供电/引脚/映射),物理存在? | ✓ pass |
| **多生产者** | 该能力有 ≥2 个独立来源,各管自己的命名空间? | ✓ pass |
| **消费者泛化** | 消费者写「我要这个能力」,不关心是谁提供的(按名字/id 取)? | ✓ pass |
| **契约跨内核稳定** | 能力语义在 Linux/RTOS/x-kernel 高度一致? | ✓ pass |
| **收益 > 成本** | 至少 N 个实现 × M 个消费者(provider trait 有足够复用面)? | ✓ pass |

> 已记录论证:clk-subsystem.md 已论证 Stage 1(晶振物理存在、CRU/SCMI/fixed-clock 多 provider、devm_clk_get 按名取、get_rate/enable 语义通用、数十 provider×数十驱动)。Stage 2 三坑也已于 2025-08 修复:name 键(去 phandle)、trait 定全(含 enable/disable/set_rate 默认)、ClkRegistry 可注入(去 kspin);Cargo.toml 已去 of/kspin 依赖(见 redline)。现为本目录唯一 Stage1+Stage2 双全的 gap。

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

## 决策与实现路径(RK3588 CRU 移植,req 确认时补全)

**决策:确认实现。** clk-framework 已 partial(core/kclk 落地);本需求覆盖 RK3588 CRU 移植
(clk-rk3588.c → drivers/integration/kdriver/src/driver_registry/clk/rk3588_cru/)所需的框架增量,
每条独立验收:

1. **F1 全局名父钟回退(kclk)**:`register_table` 解析 `ParentRef::Local(name)` 失败时回退到
   全局 provider 注册表按名解析(镜像 Linux `__clk_lookup` 全局名语义);仍解析不到 → 报错回滚
   (孤儿不放行,事务性不变)。驱动侧的外部输入钟(i2s_mclkin/hdmiphy_pixel/gmac_ptpref_io/
   dummy_aupll)由 CRU 驱动注册 0 速率占位(Linux 孤儿等价,rate 0,名字与参考一致),占位名单进缺口报告。
2. **F2 多输出控制器 provider 元数据(device-res/kdriver)**:`ClkProviderDesc` 的 fixed_rate_hz
   改为 Option(新增 `controller(id)` 构造);平台后端对 `#clock-cells >= 1` 且有 phandle 的节点
   也挂 provider desc,CRU 驱动据此取 ProviderId 注册 ClkTable。fixed-clock 驱动对无固定频率的
   desc 跳过。
3. **F3 assigned-clocks 枚举(khal/kdevice/kdriver)**:`FirmwareDevice.assigned_clocks`
   (assigned-clocks + assigned-clock-rates,#clock-cells=1 形式)逐条转
   `ResourceDesc::AssignedClock { provider, index, rate_hz }`;CRU probe 末尾按序 apply。
4. **F5 HIWORD 字段写(kclk)**:`MuxFlags::HIWORD_MASK`、`DividerFlags::HIWORD_MASK`
   (单次 `reg_write`,`(mask<<(shift+16)) | (raw<<shift)`,消除 RMW 与 gate 快路径的丢更新窗口,
   等价 Linux CLK_MUX/CLK_DIVIDER_HIWORD_MASK);`DividerFlags::READ_ONLY`(round/recalc 只读硬件)。
5. **F6 传播通知(kclk)**:SET_RATE_PARENT 向上传播改父速率时,对父节点也发 PRE/POST(失败 ABORTED)
   notifier —— Linux clk_change_rate 对每个被改节点都通知;armclk/fracmux 的换父舞步依赖它。
6. **F7 同速率短路(kclk)**:`ClkCore::set_rate` 在目标速率与当前速率一致时直接返回(不 notify、
   不写硬件),镜像 Linux `clk_core_set_rate_nolock`。U-Boot 已按 assigned 配好真机时钟,重启/复用
   assigned 值不得对在跑的 PLL/armclk 重跳舞步。
7. **RK3588 自定义 ops(驱动内,不改 kclk)**:Rk3588Pll(含 mode mux 双钟形态、锁轮询 1000µs、
   上电序列 ①-⑨ 见 portrait)、ArmClk(pre/post muxs + 父 notifier + alt 父常使能)、FracDiv
   (+子 mux remux notifier)、HalfDivComposite、GATE_LINK(常使能 linked clock)。
8. **不移植(缺口照报)**:MMC 相位 ×4(父=scmi 钟本仓库无 provider)、复位控制器/重启 notifier
   (无 reset 框架/重启钩子)。

**验收**:F1-F7 有 kclk/device-res 不变量测试;CRU 在 rk3588_defconfig 编译进镜像;真机
(OrangePi 5 Plus)probe 注册 ≈470 钟无错,clk_summary 关键钟速率与 U-Boot/DT assigned 一致;
控制台 UART2 经 CRU baudclk(=BSP id 187)解析真值并按其重编程分频,串口输出正常。

## 建议实现路径(原始模板)

按 clk 模式(provider 模式):
1. 定义 `Provider` trait(核心操作,如 `get_rate`/`enable`/`set_rate`)。
2. 建 **phandle 键的多 provider 注册表**(`SpinNoIrq<Vec<(u32, Arc<dyn Provider>)>>`),
	因为这类子系统有多个生产者(多个 controller/PHY/regulator)。
3. consumer API 走 device-res 形式:`devm_get(device, name)`,总线把 DT
	(`clocks`/`resets`/`phys`/`*-supply`)预解析成 `ResourceDesc` 变体,驱动不碰 DT。
4. DT 解析放总线层(`khal::firmware::devices`),平台 provider 在 `early_driver_init` 注册。
5. 参考:`drivers/clk/`(Tier-0 实现)。

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
