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

- **现状**：x-kernel 无通用时钟框架。RK3588 的时钟由 bootloader（U-Boot/TPL/SPL）配好，内核侧多数驱动**继承**当前时钟配置直接用。
- **阻塞**：通常 `works-around`。若驱动**主动改频率**（`clk_set_rate`，如 MMC 选卡频、I2C 定速率），则该路径 `blocker`。
- **替代**：删除所有 `clk_prepare_enable`/`clk_disable_unprepare` 调用，注释"clock assumed enabled by firmware"；频率相关逻辑用固定常量或读硬件状态推断，缺口照报。
- **报告口径**：
  > `clk` — works-around. Driver calls `clk_prepare_enable`×N and `clk_set_rate` in `set_rate`. Removed enable/disable (firmware provides clock); `set_rate` path left as TODO with `# MMC clock selection not supported` — needs clk framework (CRU) provider.

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

## 级联缺口（常见组合）

RK3588 上很多外设会撞**级联缺口**，缺口报告要把链条写清楚，方便排优先级：

- **PCIe**：`phy`（blocker）+ `clk`（CRU）+ `reset` + `iommu`(可选) + `regmap`（pipe-grf）→ 多重 blocker，需先补 phy/clk 框架。
- **USB3 (dwc3)**：`phy` + `clk` + `regulator`(PMIC) + `reset`。
- **GPU/VPU/codec**：`iommu`（blocker）+ `clk` + `regulator` + `reset`。
- **I2C controller**：i2c subsystem（blocker）+ `clk`（rate）。
- **以太网**：`phy`(MDIO) + `clk`；若用标准 `NetDevice` + `publish_net` 则主干可用，phy 走 works-around。

> 移植前先看 [`examples/dwcmshc-walkthrough.md`](../examples/dwcmshc-walkthrough.md) 里 sdmmc 如何处理 clk/phy 缺口——它是 works-around 的真实范例。
