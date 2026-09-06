<!-- 本文件由 `ldp gen-doc` 从 data/gaps.json 生成。勿手改——改 data/gaps.json 后重跑 `ldp gen-doc`。 -->

# 已知缺失子系统清单(Gap Catalog)

x-kernel 目前只实现了驱动模型的「主干」:MMIO / IRQ / coherent DMA / 同步原语 / 设备类发布。Linux 驱动里大量依赖的子系统**这里没有**。

**命令式查询**:跑 `ldp gaps [--status <s>]` 或 `ldp gaps --id <id>`。

## 阻塞程度分级

| 级别 | 含义 | 处置 |
|------|------|------|
| `blocker` | 没它驱动无法初始化/工作 | 必须先补,或明确接受「不可用」,**不得静默 stub** |
| `works-around` | 可绕过/stub,受限场景能跑 | 写明 stub 与前提,缺口照报 |
| `optional` | 锦上添花 | 缺口照报,低优先级 |

> `conditional` = 按使用维度分别定级(见各条 dimensions)。

## 逐项缺口

### clk framework (`clk-framework`)

- **状态**:`partial`
- **现状**:core/kclk 通用时钟框架已落地主线(core/kclk:两阶段 prepare/enable、全局 provider 注册表(ProviderId=DT phandle)、消费者 ClkHandle、ClkBranch 静态表事务注册、PRE/POST rate notifier、clk_summary/clk_disable_unused)。device-res 有 clk 消费/提供者契约,kdriver 平台总线按 provider-first 顺序 probe,clk-fixed-clock 驱动已接 DT fixed-clock。RK3588 CRU 是首个真实 SoC 控制器驱动。
- **Linux 符号**:`clk_disable_unprepare`, `clk_divider_ops`, `clk_fractional_divider_ops`, `clk_gate_ops`, `clk_get`, `clk_get_rate`, `clk_hw_register`, `clk_hw_register_composite`, `clk_mux_ops`, `clk_prepare_enable`, `clk_register_composite`, `clk_register_divider`, `clk_register_fixed_rate`, `clk_register_gate`, `clk_register_mux`, `clk_set_rate`, `devm_clk_hw_register`, `of_clk_add_provider`, `of_clk_src_onecell_get`
- **按维度定级**:
  - `clk-enable` (works-around): firmware 已把时钟供着,删除 enable/disable 调用,注释 'clock assumed enabled by firmware'  · 口径:clk-enable — works-around. Removed clk_prepare_enable×N (firmware provides clock).
  - `clk-rate` (conditional): provider 不可用时走 fallback(U-Boot 实测值作常量 + probe 日志标注来源 + CLKDIV 公式照搬 Linux)。RK3588 SCMI 的 ciu 直接用 RATE_GET 返回值,不 ÷2。Rockchip DT 通常只有 max-frequency 不是 clock-frequency,不能当 CLKDIV 分母。  · 口径:clk-rate — Tier-0 框架已在 RK3588 适配分支落地(drivers/clk/ 未随主线合入):clk::devm_clk_get(device,"ciu") 走 phandle 键多 provider 注册表。RK3588 sdmmc 的 ciu 经 SCMI provider(CLOCK_RATE_GET)解析到真值(~49.5 MHz)。曾因 SCMI 消息头位段记反(message_id/token)误判成'固件 stub'——实为 header bug,已修正。

### regmap (`regmap`)

- **状态**:`missing`
- **现状**:无 regmap。
- **Linux 符号**:`devm_regmap_init_mmio`, `regmap_update_bits`
- **阻塞程度**:`works-around`
- **替代**:用 Io::read32/write32 手写读-改-写,封装本地 helper fn regmap_update_bits(io, off, mask, val)。
- **报告口径**:regmap — works-around. Replaced regmap_update_bits with a local Io-based RMW helper (N call sites). No caching.

### gpio / gpiod (`gpio`)

- **状态**:`missing`
- **现状**:无 GPIO 框架、无 pinctrl。
- **Linux 符号**:`devm_gpiod_get`, `gpiod_set_value`
- **阻塞程度**:`conditional`
- **替代**:若 gpio 只是上电/复位且 firmware 已拉好电平,删调用并注释;否则 blocker 上报。
- **报告口径**:gpio (gpiod) — works-around. gpiod_set_value(reset_gpio,1) at probe removed; reset line held deasserted by firmware.

### pinctrl (`pinctrl`)

- **状态**:`missing`
- **现状**:无 pinctrl。引脚复用由 firmware 配置。
- **Linux 符号**:`pinctrl_pm_select_*`, `pinctrl`
- **阻塞程度**:`works-around`
- **替代**:删除 pinctrl 调用,注释 'pinmux configured by firmware'。
- **报告口径**:pinctrl — works-around. Pinmux configured by firmware; pinctrl calls removed.

### reset controller (`reset`)

- **状态**:`missing`
- **现状**:无 reset 框架。
- **Linux 符号**:`reset_control_assert`, `reset_control_deassert`, `devm_reset_control_get`
- **阻塞程度**:`conditional`
- **替代**:删 deassert 调用并注释 'deasserted by firmware';assert/deassert 序列留 TODO。
- **报告口径**:reset — works-around. reset_control_deassert removed; device deasserted by firmware.

### regulator (`regulator`)

- **状态**:`missing`
- **现状**:无 regulator 框架。
- **Linux 符号**:`regulator_enable`, `devm_regulator_get`
- **阻塞程度**:`conditional`
- **替代**:删调用注释 'power supplied by fixed rail';PMIC 路径 blocker 上报(通常级联 I2C→PMIC,I2C 子系统也缺)。
- **报告口径**:regulator — works-around. Fixed-rail power; regulator_enable removed. PMIC path → blocker (cascades I2C subsystem).

### iommu (`iommu`)

- **状态**:`missing`
- **现状**:无 iommu。
- **Linux 符号**:`iommu_map`, `iommu_unmap`, `arm_iommu_attach_device`
- **阻塞程度**:`conditional`
- **替代**:无;上报。RK3588 多数多媒体 IP 受此阻塞。
- **报告口径**:iommu — blocker. Device requires DMA via IOMMU; no iommu framework. RK3588 GPU/VPU/codec blocked.

### phy (`phy`)

- **状态**:`missing`
- **现状**:无 phy 框架。
- **Linux 符号**:`phy_init`, `phy_power_on`, `devm_phy_get`
- **阻塞程度**:`conditional`
- **替代**:MMC phy 用固定寄存器配置替代;PCIe/USB phy blocker 上报。
- **报告口径**:phy — blocker. PCIe/USB require powered PHY IP; no phy framework. Requires phy framework first.

### workqueue / tasklet (`workqueue`)

- **状态**:`missing`
- **现状**:无 workqueue/tasklet。
- **Linux 符号**:`INIT_WORK`, `schedule_work`, `tasklet_schedule`
- **阻塞程度**:`works-around`
- **替代**:用 ktask 起内核线程:上半部(IrqHandler)只清中断+置标志+唤醒,线程 wait_until(|| pending) 后做重活。
- **报告口径**:workqueue — works-around. Replaced schedule_work with a dedicated ktask thread woken by WaitQueue from the top-half IrqHandler.

### completion (`completion`)

- **状态**:`workaround`
- **现状**:无 Completion 类型;用 ktask::WaitQueue 替代。
- **Linux 符号**:`init_completion`, `wait_for_completion`, `complete`
- **阻塞程度**:`works-around`
- **替代**:wq.wait_timeout_until(deadline, || done.load(Ordering::Acquire));完成方 done.store(true);wq.notify_one(true)。
- **报告口径**:completion — works-around. Modeled with WaitQueue::wait_timeout_until + AtomicBool.

### 忙等延时(udelay/mdelay/ndelay) (`busy-wait`)

- **状态**:`workaround`
- **现状**:无 busy-wait helper。
- **Linux 符号**:`udelay`, `mdelay`, `ndelay`
- **阻塞程度**:`works-around`
- **替代**:core::hint::spin_loop() 自旋 + monotonic_time_nanos 截止循环(注意:关中断上下文不可用,需可抢占/进程上下文)。
- **报告口径**:udelay/mdelay — works-around. udelay(N) replaced with a monotonic_time_nanos() busy-loop (probe context, IRQs on).

### 定时器(timer_setup/mod_timer/hrtimer) (`timer`)

- **状态**:`workaround`
- **现状**:ktask 仅 register_timer_callback(很薄),无通用 hrtimer/softirq 定时器。
- **Linux 符号**:`timer_setup`, `mod_timer`, `hrtimer`
- **阻塞程度**:`works-around`
- **替代**:用内核线程 loop { sleep(Duration); do_work(); }。
- **报告口径**:timer — works-around. Periodic work modeled with a ktask loop thread.

### 设备子系统无 class(i2c/spi/watchdog/rtc/mmc-core/tty) (`device-class`)

- **状态**:`missing`
- **现状**:DeviceKind 只有 Block/Char/Net/Display/Input/Vsock/Fs9p/Bus,无 I2C-SPI-Watchdog-RTC 专用类与 publish_*。mmc controller 走自定义 BlockDevice 路径(见 sdmmc),无 mmc-core 协议栈复用。
- **Linux 符号**:`i2c_add_adapter`, `spi_register_master`
- **阻塞程度**:`conditional`
- **替代**:能套 Block/Char/Net 的就套(watchdog→char);套不上的(i2c/spi controller)→ blocker,需先建子系统。
- **报告口径**:i2c subsystem — blocker. No DeviceKind::I2c / publish_i2c; controller can probe and init registers but cannot register an I2C bus for child devices. Requires building the i2c class first.

### 其他(vmalloc/RCU/kmem_cache/leds/pm_runtime) (`misc`)

- **状态**:`workaround`
- **现状**:vmalloc 无(大缓冲用 devm_alloc_coherent 或 alloc_pages);RCU 无(用 Arc/RwLock 替代);kmem_cache/slab 无(用 Box/Vec);leds/input 框架薄;pm_runtime 仅 DeviceDriver::suspend/resume。
- **Linux 符号**:`vmalloc`, `RCU`, `kmem_cache_create`
- **阻塞程度**:`optional`
- **替代**:vmalloc→devm_alloc_coherent/alloc_pages;RCU→Arc/RwLock;kmem_cache→Box/Vec;pm_runtime→删 runtime pm 调用保留系统级 suspend/resume。
- **报告口径**:misc — optional/works-around per item (vmalloc/RCU/kmem_cache/pm_runtime).

## 级联缺口(常见组合)

| 区域 | 缺口链 |
|------|--------|
| PCIe | phy(blocker) + clk(CRU) + reset + iommu(可选) + regmap(pipe-grf) |
| USB3 (dwc3) | phy + clk + regulator(PMIC) + reset |
| GPU/VPU/codec | iommu(blocker) + clk + regulator + reset |
| I2C controller | i2c subsystem(blocker) + clk(rate) |
| 以太网 | phy(MDIO) + clk |

---

> 移植前先看 `examples/dwcmshc-walkthrough.md` 里 sdmmc 如何处理 clk/phy 缺口——它是 works-around 的真实范例。

## 参考：真机 bring-up 易踩的坑（gen-doc 原样保留）

以下为**硬件现实**类叙事（非子系统缺口，但会让“编译通过”的驱动在硬件上挂掉），由 gen-doc 的受保护附录块承载。
若要增删，编辑本文件 `GEN-DOC:APPENDIX` 标记之间的内容。

<!-- GEN-DOC:APPENDIX-START (受保护参考块,gen-doc 原样保留) -->

这些不是“缺哪个子系统”，而是**硬件现实**——移植时稍不留意就会让 P5 全绿、P6 挂掉。dw_mmc 移植一次踩中三个。Bring-up 时对照 workflow/port-workflow.md §6 逐条排查。

### PWREN / 卡上电使能（`PWREN` 寄存器）

- **坑**：dw_mshc 的 `PWREN` 输出控制卡槽 vmmc 供电。Linux 在 `set_ios` 的 `MMC_POWER_UP` 写 `PWREN |= 1<<slot`，U-Boot `dwmci_init` 第二行就写 `PWREN=1`。**漏写 = 卡不上电 = 所有 CMD `RTO`**。
- **特别地**：DT 里有 `vmmc-supply` 说明供电是可开关的，**必须**显式使能；CTRL reset 不会清 PWREN，所以像 U-Boot 那样在 init 开头写一次即可。
- **处置**：probe 初始化序列**第一步**写 `PWREN=1`。
- **报告口径**：`PWREN` — works-around. Explicit `PWREN=1` at probe step 1 (per U-Boot dwmci_init).

### CDETECT（native card detect）不可信

- **坑**：控制器自带的 `CDETECT` 卡检测在很多 RK 板子上**不可靠**——OPi5+ 上 sdmmc 和 sdio 两个节点都读“有卡”（包括没插卡的那个）。原因：板子用 GPIO CD（`cd-gpios`）或没接 CDETECT 引脚。
- **处置**：**不要**拿 `CDETECT` 当“卡在不在”的硬门去 abort probe；最多当快速跳过路径。真要区分“无卡”靠 CMD 超时（`RTO`）。Linux 也是 `dw_mci_get_cd` 先查 GPIO CD，CDETECT 只是兏底。

### 一个 `compatible` 命中多个节点（sdmmc + sdio）

- **坑**：RK3588 的 sdmmc 和 sdio 节点 `compatible` 相同（`rockchip,rk3588-dw-mshc`）。驱动会**对两个节点都 probe**，其中没插卡的那个必然失败。
- **处置**：**逐节点容忍失败**——没设备的节点失败是正常的。在胶水层按节点 `device.location()` 打日志区分；无卡节点返回错误即可。真要只绑 SD 卡槽，可按 MMIO 基址过滤（OPi5+ sdmmc=`0xfe2c0000`、sdio=`0xfe2d0000`），但属板级硬编码，慎用。

### CTRL 的 DMA 位（把 PIO 驱动跑在“设计走 DMA 的 IP”上）

- **坑**：dw_mshc 的 `HCON TRANS_MODE` 决定数据通路——RK3588 是 `0`=内部 IDMAC，**U-Boot 和 Linux 都走 DMA**。U-Boot 跑过会把 `CTRL.USE_IDMAC`(bit25) 和 `CTRL.DMA_ENABLE`(bit5) 置位；而 `ctrl_reset(ALL_RESET_FLAGS)` 里的 `DMA_RESET` 只是**脉冲式复位 DMA 逻辑、不清这两个配置位**。于是 FIFO 输出仍被路由给“未配置的 DMA 引擎”：PIO 读数据寄存器（`0x200`）**永远不出栈**，`DATA_OVER` 永远不置位，且**不报任何数据错误**——表现是无声死循环/超时。指纹：命令/时钟/`BYTCNT`/`BLKSIZ` 全对、数据照进 FIFO（`FCNT` 在涨）、FIFO 满着、`RINTSTS` 只有 `RXDR`、无 `DATA_OVER`、无错误位。
- **处置**：init 里显式清这两位（`CTRL &= ~(USE_IDMAC|DMA_ENABLE)`），对照 Linux `dw_mci_submit_data` 进 PIO 前的清理。**别假设 `ctrl_reset` 会清配置位**——它只发复位脉冲。
- **诊断手法**：卡空闲时连读几次数据寄存器看 `FCNT` 降不降（“pop-test”）；不降 = 读不出栈。
- **报告口径**：`PIO vs IDMAC` — works-around（驱动侧已绕过）。RK3588 dw_mshc `TRANS_MODE=IDMAC`（U-Boot/Linux 走 DMA）；本驱动用 PIO 轮询，**init 显式清 `CTRL.USE_IDMAC/DMA_ENABLE`**（U-Boot 残留，`ctrl_reset` 不清）使 FIFO 输出回到 PIO 口。无 IDMAC 描述符/scatter-gather，性能受限；高速/UHS 需补 IDMAC。
<!-- GEN-DOC:APPENDIX-END -->
