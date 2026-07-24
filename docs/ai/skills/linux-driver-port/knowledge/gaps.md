# 已知缺失子系统清单（Gap Catalog）

x-kernel 目前只实现了驱动模型的"主干"：MMIO / IRQ / coherent DMA / 同步原语 / 设备类发布。Linux 驱动里大量依赖的子系统**这里没有**。本文件给出每个缺失项的现状、阻塞程度、可行替代、以及缺口报告该怎么写。

## 阻塞程度分级（gap-report 里每条都要标）

| 级别 | 含义 | 处置 |
|------|------|------|
| `blocker` | 没它驱动**无法初始化或无法工作**（如 SoC 外设没时钟不转） | 必须先补该子系统，或明确接受"此驱动在当前内核上不可用"，**不得静默 stub** |
| `works-around` | 可绕过/stub，驱动能在**受限场景**跑（如先假设 phy 固定配置） | 在驱动里写明 stub 与前提条件，缺口照报 |
| `optional` | 锦上添花，缺了基本功能仍可用（如 pm_runtime 精细化、LED） | 缺口照报，可低优先级 |

## 判断框架（遇到一个本表没列的依赖怎么定级）

1. 它是**初始化必需**资源吗（clk/reset/gpio enable/regulator 供电）？→ 多半 `blocker`。
2. 它是**性能/特性增强**吗（pm_runtime、regmap 缓存、pinctrl 复用）？→ 多半 `optional` 或 `works-around`。
3. 它能**用现有原语手写等效逻辑**吗（regmap→`Io` 读改写、completion→`WaitQueue`）？→ `works-around`，给出替代代码。
4. 拿不准就按 `blocker` 上报，交人决策。

---

## 逐项缺口

### clk framework（`clk_get` / `clk_prepare_enable` / `clk_set_rate` / `clk_get_rate`）

clk 要按**两个独立维度**分别定级——这是 dw_mmc 移植踩过的真坑（见 [dw_mmc-walkthrough 复盘](../examples/dw_mmc-walkthrough.md#复盘真机-io-失败从预言到验证)）：把整个 clk 笼统标 `works-around`，会让"必须知道 CIU 速率才能算 CLKDIV"这件事被"firmware 供着时钟"这句话掩盖。

- **现状**：x-kernel 无通用时钟框架。RK3588 的时钟由 bootloader（U-Boot/TPL/SPL）配好，内核侧多数驱动**继承**当前时钟配置直接用。

#### 维度 A：clk-enable（`clk_prepare_enable` / `clk_disable_unprepare`）

- **阻塞**：`works-around`。firmware 已把时钟供着，删掉 enable/disable 即可，注释 "clock assumed enabled by firmware"。
- **替代**：删除所有 `clk_prepare_enable`/`clk_disable_unprepare` 调用。

#### 维度 B：clk-rate（`clk_get_rate` / `clk_set_rate`）—— **最容易被误判为 works-around**

驱动**算硬件分频**时需要知道输入时钟速率本身，例如：MMC 的 `CLKDIV = f(ciu_rate, target_card_clock)`、I2C 的 SCL 分频、UART 的 baud divisor、SPI 的速率。**这跟"时钟开没开"是两回事。**

- **阻塞**：分两种——
  - 能**取到真值**（DT 有 `clock-frequency`，或可从硬件状态推断）→ `works-around`：读出来用，缺口照报。
  - **取不到真值**（DT 无 `clock-frequency`、又无 clk 框架读 `clk_get_rate`）→ 这是 **smoke-path blocker**：分频算错 → 卡识别/通信失败。**不得用一个"看起来合理"的常量静默糊弄过去当成 works-around。**
- **Rockchip 特别注意**（OPi5+ 实测）：
  - RK 的 DT 里 sdmmc 节点**通常只有 `max-frequency`，没有 `clock-frequency`**。`max-frequency` 是"卡时钟上限"（caps），**不是** CIU 输入速率——**绝对不能拿它当 CLKDIV 的分母**。Linux 走 `clk_get_rate(ciu_clk)`，再除 `RK3288_CLKGEN_DIV(=2)` 得到 `host->bus_hz`。
  - 无 clk 框架时，唯一可靠来源是**对照 U-Boot**：在目标板上跑 U-Boot 的 `mmc info`（或等价），读它报的 Bus Speed / bus_hz，作为常量并**在 probe 时打印**，方便核对。
- **替代（取不到真值时）**：用 U-Boot 实测值作常量 + probe 日志标注来源 + 在缺口报告里写明"smoke-blocker，需 clk(CRU) provider 校准"；**CLKDIV 公式照搬 Linux**（含 Rockchip 的 ÷2，见 [api-mapping §6](api-mapping.md)）。
- **报告口径**：
  > `clk-enable` — works-around. Removed `clk_prepare_enable`×N (firmware provides clock).
  > `clk-rate` — **smoke-blocker** (ci_clk). No clk framework to read `clk_get_rate(ciu_clk)`; DT has no `clock-frequency` (only `max-frequency`, which is NOT the CIU rate). CLKDIV computed from a hardcoded `bus_hz=50MHz` (U-Boot `mmc info` value, logged at probe) using the Linux formula incl. RK3288_CLKGEN_DIV. Card-ID clock ≈400 kHz is correct; high-speed correctness depends on this guess — must calibrate via clk(CRU) provider.

### regmap（`devm_regmap_init_mmio` / `regmap_update_bits`）

- **现状**：无 regmap。
- **阻塞**：`works-around`。
- **替代**：用 `Io::read32`/`write32` 手写读-改-写，封装一个本地 helper：
  ```rust
  fn regmap_update_bits(io: &Io, off: usize, mask: u32, val: u32) {
      let mut v = io.read32(off);
      v = (v & !mask) | (val & mask);
      io.write32(off, v);
  }
  ```
- **报告口径**：
  > `regmap` — works-around. Replaced `regmap_update_bits` with a local `Io`-based RMW helper (5 call sites). No caching.

### gpio / gpiod（`devm_gpiod_get` / `gpiod_set_value`）

- **现状**：无 GPIO 框架、无 pinctrl。
- **阻塞**：视用途。电源使能/复位引脚（`gpiod_set_value` 控制上电/复位时序）→ 通常 `works-around`（引脚已由 firmware 配好电平）；作为功能引脚（中断、bus）→ `blocker`。
- **替代**：若该 gpio 只是上电/复位且 firmware 已拉好电平，删调用并注释；否则 `blocker` 上报。
- **报告口径**：
  > `gpio (gpiod)` — works-around. `gpiod_set_value(reset_gpio, 1)` at probe removed; reset line held deasserted by firmware. Power-enable GPIO on suspend path → blocker (see gap below).

### pinctrl（`pinctrl_pm_select_*` / `pinctrl` DT node）

- **现状**：无 pinctrl。引脚复用由 firmware 配置。
- **阻塞**：`works-around`（firmware 已 mux）；`optional` 多数情况。
- **替代**：删除 pinctrl 调用，注释"pinmux configured by firmware"。

### reset controller（`reset_control_assert/deassert` / `devm_reset_control_get`）

- **现状**：无 reset 框架。
- **阻塞**：视是否需要**运行时复位**。多数 SoC 外设 probe 时已解除复位 → `works-around`；需主动 assert/deassert 复位时序的 → `blocker`。
- **替代**：删 deassert 调用并注释"deasserted by firmware"；assert/deassert 序列留 TODO。

### regulator（`regulator_enable` / `devm_regulator_get`）

- **现状**：无 regulator 框架。
- **阻塞**：PMIC 供电的外设（RK3588 部分模块经 RK806 PMIC 供电）→ `blocker`（内核侧无法上电）；板载固定供电 → `works-around`。
- **替代**：删调用注释"power supplied by fixed rail"；PMIC 路径 `blocker` 上报（通常依赖 I2C→PMIC，而 I2C 子系统本身也缺，级联缺口）。

### iommu（`iommu_map/unmap` / `arm_iommu_attach_device`）

- **现状**：无 iommu。
- **阻塞**：需要 DMA 且设备经 IOMMU 隔离的（RK3588 GPU/VPU/codec）→ `blocker`；直连 DMA（MMC/网卡）→ 不涉及。
- **替代**：无；上报。RK3588 多数多媒体 IP 受此阻塞。

### phy（`phy_init` / `phy_power_on` / `devm_phy_get`）

- **现状**：无 phy 框架。
- **阻塞**：PCIe / USB3 / SATA 等**带独立 PHY IP** 的 → `blocker`（PHY 不上电链路不起来）；MMC 的 phy 多为简单延迟配置 → `works-around`。
- **替代**：MMC phy 用固定寄存器配置替代；PCIe/USB phy `blocker` 上报。

### workqueue / tasklet（`INIT_WORK` / `schedule_work` / `tasklet_schedule`）

- **现状**：无 workqueue/tasklet。
- **阻塞**：中断下半部/延后处理 → `works-around`（可自建内核线程）或 `optional`。
- **替代**：用 `ktask` 起一个内核线程，上半部（`IrqHandler`）只清中断+置标志+唤醒线程，线程做重活：
  ```rust
  // IrqHandler: 清中断 → wq.notify_one(true)
  // worker 线程: wq.wait_until(|| pending) → 处理
  ```
- **报告口径**：
  > `workqueue` — works-around. Replaced `schedule_work(&priv->rx_work)` with a dedicated `ktask` thread woken by a `WaitQueue` from the top-half `IrqHandler`.

### completion（`init_completion` / `wait_for_completion` / `complete`）

- **现状**：无 `Completion` 类型。
- **阻塞**：`works-around`。
- **替代**：用 `ktask::WaitQueue` + `AtomicBool` 标志模拟：
  ```rust
  wq.wait_timeout_until(deadline, || done.load(Ordering::Acquire))
  // 完成方: done.store(true, …); wq.notify_one(true);
  ```
- **报告口径**：
  > `completion` — works-around. Modeled probe-time "wait for first IRQ" with `WaitQueue::wait_timeout_until` + `AtomicBool`.

### 忙等延时（`udelay` / `mdelay` / `ndelay`）

- **现状**：无 busy-wait helper。
- **阻塞**：复位/PLL 稳定时序里 `udelay(N)` → `works-around`。
- **替代**：短延时用 `core::hint::spin_loop()` 自旋 + 基于 `khal::time::monotonic_time_nanos()` 的截止循环（注意：自旋时关中断上下文不可用，需在可抢占/进程上下文）。缺口记录。
- **报告口径**：
  > `udelay/mdelay` — works-around. `udelay(100)` after reset replaced with a `monotonic_time_nanos()` busy-loop (probe context, IRQs on).

### 定时器（`timer_setup` / `mod_timer` / `hrtimer`）

- **现状**：`ktask` 仅 `register_timer_callback`（很薄），无通用 hrtimer/softirq 定时器。
- **阻塞**：周期性看门狗喂狗、超时轮询 → `works-around`（内核线程 + sleep）；精确一次性定时 → 记录。
- **替代**：用内核线程 `loop { sleep(Duration); do_work(); }`。

### 设备子系统无 class（i2c / spi / watchdog / rtc / mmc-core / tty）

- **现状**：`DeviceKind` 只有 `Block/Char/Net/Display/Input/Vsock/Fs9p/Bus`，**无** I2C/SPI/Watchdog/RTC 专用类与 `publish_*`。
- **阻塞**：
  - **i2c controller**：`blocker`（无 `publish_i2c`，无法挂 I2C 总线及从设备）。
  - **watchdog**：可降级为 `works-around`（做成 char 设备 `publish_char`，喂狗经 ioctl/write，但无统一 watchdog 子系统）。
  - **rtc**：`works-around`（char 设备）。
  - **mmc controller**：项目走自定义 `BlockDevice` 路径（见 sdmmc），无 mmc-core 协议栈复用。
- **替代**：能套 `Block/Char/Net` 的就套（watchdog→char）；套不上的（i2c/spi controller）→ `blocker`，需先建子系统。
- **报告口径**：
  > `i2c subsystem` — blocker. No `DeviceKind::I2c` / `publish_i2c`; controller can probe and init registers but cannot register an I2C bus for child devices. Requires building the i2c class first.

### 其他

| 子系统 | 现状 | 阻塞 | 替代 |
|--------|------|------|------|
| `vmalloc` | 无 | works-around | 大缓冲用 `devm_alloc_coherent` 或 `alloc_pages` |
| `RCU` | 无 | works-around | `Arc`/`RwLock` 替代 |
| `kmem_cache`（slab） | 无 | optional | `Box`/`Vec` |
| `leds` / `input` event | 部分（`DeviceKind::Input` 存在但框架薄） | optional | 视情况上报 |
| `pm_runtime` | 仅 `DeviceDriver::suspend/resume` | optional | 删 runtime pm 调用，保留系统级 suspend/resume |

---

## 真机 bring-up 易踩的坑（非子系统缺口，但会让"编译通过"的驱动在硬件上挂掉）

这些不是"缺哪个子系统"，而是**硬件现实**——移植时稍不留意就会让 P5 全绿、P6 挂掉。dw_mmc 移植一次踩中三个。Bring-up 时对照 [Phase 6](../workflow/port-workflow.md#phase-6--带机校验与-triagebring-up--triage) 逐条排查。

### PWREN / 卡上电使能（`PWREN` 寄存器）

- **坑**：dw_mshc 的 `PWREN` 输出控制卡槽 vmmc 供电。Linux 在 `set_ios` 的 `MMC_POWER_UP` 写 `PWREN |= 1<<slot`，U-Boot `dwmci_init` 第二行就写 `PWREN=1`。**漏写 = 卡不上电 = 所有 CMD `RTO`**。
- **特别地**：DT 里有 `vmmc-supply`（如 OPi5+ 的 sdmmc）说明供电是可开关的，**必须**显式使能；CTRL reset 不会清 PWREN（两份参考都在 reset 前写它且能跑），所以像 U-Boot 那样在 init 开头写一次即可。
- **处置**：probe 初始化序列**第一步**写 `PWREN=1`（对照参考驱动，见 [port-workflow P1 编号序列](../workflow/port-workflow.md)）。

### CDETECT（native card detect）不可信

- **坑**：控制器自带的 `CDETECT` 卡检测在很多 RK 板子上**不可靠**——OPi5+ 上 sdmmc 和 sdio 两个节点都读"有卡"（包括没插卡的那个）。原因：板子用 GPIO CD（`cd-gpios`）或干脆没接 CDETECT 引脚。
- **处置**：**不要**拿 `CDETECT` 当"卡在不在"的硬门去 abort probe；最多当快速跳过路径。真要区分"无卡"靠 CMD 超时（`RTO`），并在日志里写清。Linux 也是 `dw_mci_get_cd` 先查 GPIO CD，CDETECT 只是兜底。

### 一个 `compatible` 命中多个节点（sdmmc + sdio）

- **坑**：RK3588 的 sdmmc 和 sdio 节点 `compatible` 相同（`rockchip,rk3588-dw-mshc`）。驱动会**对两个节点都 probe**，其中没插卡的那个必然失败。
- **处置**：**逐节点容忍失败**——没设备的节点失败是正常的，不要当成"驱动坏了"。在胶水层按节点 `device.location()` 打日志区分；无卡节点返回错误（`Io`/`Unsupported`）即可，别让它的失败噪音盖住真正该工作的节点。真要只绑 SD 卡槽，可按 MMIO 基址（`first_mmio().base`，OPi5+ sdmmc=`0xfe2c0000`、sdio=`0xfe2d0000`）过滤，但那属于板级硬编码，慎用。

### CTRL 的 DMA 位（把 PIO 驱动跑在"设计走 DMA 的 IP"上）

- **坑**：dw_mshc 的 `HCON TRANS_MODE` 决定数据通路——RK3588 是 `0`=内部 IDMAC，**U-Boot 和 Linux 都走 DMA**。U-Boot 跑过会把 `CTRL.USE_IDMAC`(bit25) 和 `CTRL.DMA_ENABLE`(bit5) 置位；而 `ctrl_reset(ALL_RESET_FLAGS)` 里的 `DMA_RESET` 只是**脉冲式复位 DMA 逻辑、不清这两个配置位**。于是 FIFO 输出仍被路由给"未配置的 DMA 引擎"：PIO 读数据寄存器（`0x200`）**永远不出栈**，`DATA_OVER` 永远不置位，且**不报任何数据错误**——表现是无声死循环/超时。指纹：命令/时钟/`BYTCNT`/`BLKSIZ` 全对、数据照进 FIFO（`FCNT` 在涨）、FIFO 满着、`RINTSTS` 只有 `RXDR`、无 `DATA_OVER`、无错误位。
- **处置**：init 里显式清这两位（`CTRL &= ~(USE_IDMAC|DMA_ENABLE)`），对照 Linux `dw_mci_submit_data` 进 PIO 前的清理。**别假设 `ctrl_reset` 会清配置位**——它只发复位脉冲。
- **诊断手法**：卡空闲（传输已停）时连读几次数据寄存器看 `FCNT` 降不降（"pop-test"）；不降 = 读不出栈。定位到"读不出栈"后，对照 Linux 源码看 PIO 进入时还额外清了什么 → 锁定 DMA 位。
- **报告口径**：
  > `PIO vs IDMAC` — works-around（驱动侧已绕过）。RK3588 dw_mshc `TRANS_MODE=IDMAC`（U-Boot/Linux 走 DMA）；本驱动用 PIO 轮询，**init 显式清 `CTRL.USE_IDMAC/DMA_ENABLE`**（U-Boot 残留，`ctrl_reset` 不清）使 FIFO 输出回到 PIO 口。无 IDMAC 描述符/scatter-gather，性能受限；高速/UHS 需补 IDMAC。

## 级联缺口（常见组合）

RK3588 上很多外设会撞**级联缺口**，缺口报告要把链条写清楚，方便排优先级：

- **PCIe**：`phy`（blocker）+ `clk`（CRU）+ `reset` + `iommu`(可选) + `regmap`（pipe-grf）→ 多重 blocker，需先补 phy/clk 框架。
- **USB3 (dwc3)**：`phy` + `clk` + `regulator`(PMIC) + `reset`。
- **GPU/VPU/codec**：`iommu`（blocker）+ `clk` + `regulator` + `reset`。
- **I2C controller**：i2c subsystem（blocker）+ `clk`（rate）。
- **以太网**：`phy`(MDIO) + `clk`；若用标准 `NetDevice` + `publish_net` 则主干可用，phy 走 works-around。

> 移植前先看 [`examples/dwcmshc-walkthrough.md`](../examples/dwcmshc-walkthrough.md) 里 sdmmc 如何处理 clk/phy 缺口——它是 works-around 的真实范例。
