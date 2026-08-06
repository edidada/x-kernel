# clk 子系统（Tier 0：速率查询）

x-kernel 已落地 **Tier-0** 的中性时钟框架 `drivers/clk/`，外加 RK3588 的 SCMI provider。移植 SoC 外设遇到 `clk_get_rate` 时先读这里，再回 [`gaps.md`](gaps.md) 的 clk 条目。

## 分层（平台中性是关键）

```text
drivers/clk/                              中性框架（所有平台共用；**OS-agnostic**，不依赖任何 k* / of crate）
  ClkProvider trait                       get_rate + enable/disable/set_rate/is_enabled（Tier 0 给默认实现，锁死形状）
  ClkRegistry（可注入）+ register_provider   name -> Arc<dyn ClkProvider>（多 provider；core-only 自旋锁，无 kspin）
  Clk 句柄 + clk::devm_clk_get(device, name)  消费者 API（device-res 形式）
  FixedClockProvider                      纯实现（rate 固定，无 DT 依赖）
drivers/kdriver/src/clk_fixed.rs         register_fixed_clocks（DT 遍历在此，不进 clk crate）
platforms/kplat-aarch64/.../scmi.rs       SoC/固件专属 provider（SCMI-over-SMC，AArch64 only）
```

> **OS-agnostic 红线**：`drivers/clk/` 的 `Cargo.toml` 不得依赖任何 `k*` crate（kspin 等）或 `of`（DT）crate——这些是固件描述/宿主锁 concern，由上层（kdriver 的 DT 遍历、bus 层的 DT 解析）承担。这样 clk 能被任意内核/固件后端复用。中性用 **provider name**（DT 路径=节点名去 `@unit`）作注册键，而非 phandle（phandle 是 DT 特有，ACPI/PCI 平台没有）。

> 与 `device-res` 的关系：时钟和 MMIO/IRQ/DMA 一样走 **Desc + devm** 形式——总线把 `clocks`/`clock-names` 预解析成 `device_res::ClkDesc`（`ResourceDesc::Clock` 变体，带 `provider: &'static str`），驱动用 `clk::devm_clk_get(device, name)` 取。与 MMIO/IRQ/DMA 的区别：后者是**单全局** provider，而时钟有**多个**生产者（CRU、SCMI、每个晶振……），所以 clk 注册表是 **name 键的多 provider 表**，且做成可注入 `ClkRegistry`（进程全局默认 + 测试可自建，不再需 `reset_registry`）。

判定一条东西放哪层：
- 跟 SoC 寄存器/PLL/分频表绑死 → **按平台门控**的 SoC 层（如 RK3588 CRU、`kplat-aarch64/.../scmi.rs`）。
- DT/行业**标准的跨平台** binding（`fixed-clock`、未来的 SCMI 客户端）→ 中性层，一次建成服务所有平台。
- 跟硬件无关的（trait、注册表、消费者 API）→ 中性层；DT 解析（`clocks`/`clock-names` → `ClkDesc`）在**总线层**（`khal::firmware::devices`）。

## 消费者怎么用（device-res 形式）

驱动 probe 拿到的是 `Arc<DeviceObject>`。时钟和 MMIO 一样已被**总线在枚举时预解析**进设备的 `resources`（`ResourceDesc::Clock`），所以驱动按名取时钟即可——**不碰 DT，不需要找回 DT 节点**：

```rust
// 在 probe_device(device: Arc<DeviceObject>) 里：
let bus_hz = clk::devm_clk_get(device.as_ref(), "ciu")   // ResResult<Clk>
    .ok()
    .and_then(|clk| clk.rate())                          // Option<u64>
    .unwrap_or(FALLBACK_HZ) as u32;
```

- `devm_clk_get` 在设备的 `resources()` 里按名找 `ResourceDesc::Clock`，拿 `desc.provider`（OS 中性 name）查 clk 的多 provider 注册表，返回 `Clk` 句柄；`.rate()` 查速率。找不到该时钟 → `Err(InvalidResource)`；没注册 provider → `Err(NoProvider)`。
- **驱动不碰 DT，也不需要 provider 的 phandle**——phandle→name 的翻译在总线层（`khal::firmware::devices` 解析 `clocks` 时用 `of::node_name_key` 去后缀）一次性完成。没有 `find_node_by_reg_base` 之类的 hack。
- 注册时机：`kdriver::init_drivers()` 里 `clk_fixed::register_fixed_clocks()`；SoC provider（SCMI）在平台 init（kplat `early_driver_init`）用 `of::node_name_key(protocol.name)` 注册。都在消费者 probe 之前。

样例：[kdriver/.../block/sdmmc.rs](../../../../../drivers/kdriver/src/driver_registry/block/sdmmc.rs) 的 `resolve_ciu_rate`。

## Tier 0 / 1 / 2

| Tier | 范围 | 状态 | 何时需要 |
|------|------|------|---------|
| **Tier 0** | 只读 `get_rate` | ✅ 已实现 | 现在就该做：退役驱动里硬编码的时钟常量（sdmmc `bus_hz`、UART baud divisor…） |
| **Tier 1** | gate/divider/mux 原语 + enable/set_rate | ❌ | 需 CRU MMIO 时钟、eMMC/SDIO 动态调速时（~1–2k 行） |
| **Tier 2** | 完整 CCF（速率变更树遍历、PLL、notifier、`clk-rk3588` 表） | ❌ | 仅 CPU 变频 / 激进 runtime PM（~10–15k 行） |

## RK3588 SCMI 实测（真实根因：header 位段，不是固件墙）

sdmmc 的 `ciu` 是 SCMI 时钟（DT `clocks = <&scmi_clk 9>`，`#clock-cells=<1>`）。RK3588 的 SCMI-over-SMC 走 `firmware/scmi`（`arm,smc-id=0x82000010`），共享内存在 PMU SRAM `0x10f000`（DT `arm,scmi-shmem` 节点，经 `mmio-sram` 父节点 ranges 翻译）。SCMI provider 在 [platforms/kplat-aarch64/src/peripherals/scmi.rs](../../../../../platforms/kplat-aarch64/src/peripherals/scmi.rs)（SMC 门铃 + shmem 组帧 + `CLOCK_RATE_GET`）。

**根因（Orange Pi 5 Plus）——固件是好的，是 header 位段错了。** 另一个内核（ax_driver）在同一块板上 `CLOCK_RATE_GET(0x9)` 实打实返回 49500000 Hz。问题出在 SCMI **消息头的位段布局**：一度把 `message_id` 放在 `[25:18]`、`token` 放在 `[9:0]`（记反了规范）。正确布局（ARM SCMI spec / Linux `pack_scmi_header` / ax_driver `MsgHeader::pack()`）是：

- `message_id` **`[7:0]`**
- `message_type` `[9:8]`
- `protocol_id` `[17:10]`
- `token` **`[27:18]`**

**为什么被 VERSION 骗了很久**：VERSION 的 `message_id=0`、`token=0`，**错的和对的两种位段布局算出的 header 完全一样** → VERSION 一直通。但凡 `message_id≠0`，TF-A 在 `[7:0]` 读到 0，就把命令当成 VERSION 处理：带 payload 的 → `PROTOCOL_ERROR(-10)`；`PROTOCOL_ATTRIBUTES`（msg 1）→ TF-A 当成 VERSION，回 `0x20000`。**不是固件 stub——是 TF-A 一直正确地把我发错位的东西当 VERSION 应答。** 这误导出了"只有 VERSION 被应答 = 固件桩"的错判（见下节修正）。

修复（`message_id[7:0] | protocol_id[17:10] | token[27:18]`、SMC 前后 `dsb sy;isb` 屏障、`RATE_GET` 返回值直接作 ciu 参考时钟**不 ÷2**）后，`ciu` 经 SCMI 解析到真值。**现成参考**：ax_driver 的 `arm-scmi-rs`（`MsgHeader::pack()` + `MSG_*_MASK`、shmem 布局、`dsb sy` 屏障、`RATE_GET` 直用）——照它对齐最稳。

### 诊断"我的 SCMI 代码错了" vs "固件是桩" —— ⚠ 当心 VERSION 会骗你

带机撞到 SCMI 错误时的探针阶梯（消费者首次调用 `log::warn!` 一次）：

| 探针 | 含义 |
|------|------|
| base `VERSION`（proto `0x10`, msg `0x0`, 无 payload） | 通 → SMC / shmem 传输对；**但不证明 header 位段对**（见下） |
| 目标协议 `VERSION`（如 clock proto `0x14`, msg `0x0`） | 通 → 该协议路由对 |
| base `MESSAGE_ATTRIBUTES`（proto `0x10`, msg `0x2`, **带 payload**） | 通 → payload 帧格式对（规范强制命令） |
| 目标命令（如 clock `RATE_GET` proto `0x14`, msg `0x6`, 带 payload） | 真正想做的 |

判读（**重要**：VERSION 通 ≠ header 位段对）：

- base `VERSION` 通、但其它命令（尤其带 payload 的）挂 → **先怀疑自己的 header 位段，不要立刻断定固件 stub**。VERSION 的 `msg=0/token=0` 会**掩盖**位段错误：把 `message_id`/`token` 的位段放反，VERSION header 仍然算对（因为 0 放哪都是 0），于是 VERSION 照常通，其它命令全挂——**症状和"固件只实现 VERSION"几乎一模一样**。本次移植就是被这样骗的。
- **判据**：拿一个**已知能用的 SCMI 实现**逐位核对 header 编码，或直接对照掩码：`MSG_ID_MASK=GENMASK(7,0)`、`MSG_TYPE_MASK=GENMASK(9,8)`、`MSG_PROTOCOL_ID_MASK=GENMASK(17,10)`、`MSG_TOKEN_ID_MASK=GENMASK(27,18)`。位段错是最隐蔽的一类 bug。
- 几个非 VERSION 命令（含 `PROTOCOL_ATTRIBUTES`/`MESSAGE_ATTRIBUTES`）都通、唯独目标命令挂 → 才是"固件不支持该命令"。
- 真正的固件 stub（只实现 VERSION）**存在但很罕见**；认定它之前，**必须**先用已知正确的 header 编码排除自己这边。
- 响应 `length`：错误响应 `len = 8` = `header(4)+status(4)`，证明 length 约定"含 header"，可核对请求 length。

## 常见坑

- **`early_driver_init` 在 logger 之前跑**：`khal::early_driver_init()`（kruntime:311）早于"Logging is enabled"（:322），这里的 `log::*` **全部静默**。平台 init 诊断用 `kernel_boot::bootln!`（走 boot 控制台，日志里带 `[boot]` 前缀）。
- **`of::resolve_node("别名")` 可能不稳**：曾遇到 `resolve_node("scmi_clk")` 在某 DT 上返回 None（别名/路径解析脆弱）。稳健做法是**按结构找**——`fdt.all_nodes().find(|n| n 有某属性 && 其父 compatible 是 X)`，不依赖别名或硬编码路径。
- **shmem 物理地址**：`arm,scmi-shmem` 的 `reg` 是相对 `mmio-sram` 父节点的偏移；绝对地址 = 父节点基址 + 偏移（PMU SRAM 是 1:1 平铺映射）。别只读子节点 reg 当绝对地址。
- **⚠ SCMI header 位段（最大的坑）**：消息头是 `message_id[7:0] | message_type[9:8] | protocol_id[17:10] | token[27:18]`。**记反了 `message_id`/`token` 的位置时，VERSION 照样通**（msg=0、token=0，放哪都是 0），于是 VERSION 一直成功、其它命令全 `PROTOCOL_ERROR`，症状极像"固件 stub"。**先对照已知实现**（ax_driver `MsgHeader::pack()` / Linux `pack_scmi_header`，掩码 `MSG_ID_MASK=GENMASK(7,0)`、`MSG_TOKEN_ID_MASK=GENMASK(27,18)`）核对，再怀疑固件。SMC 前后加 `dsb sy;isb` 屏障；`CLOCK_RATE_GET` 返回值直接当 ciu 参考时钟用，**不要 ÷2**（`RK3288_CLKGEN_DIV` 是 rk3288 时代的，RK3588 SCMI 不适用）。

## 报告口径

> `clk-rate` — Tier-0 框架已落地（`drivers/clk/`）：`clk::devm_clk_get(device,"ciu")` 走 **name 键**多 provider 注册表（时钟经总线预解析为 `ResourceDesc::Clock`，provider name 由总线从 DT 节点名去 `@unit` 得出，驱动/clk crate 都不碰 phandle）。RK3588 sdmmc 的 `ciu` 经 SCMI provider 查询（`CLOCK_RATE_GET`），返回 ciu 参考时钟（~49.5 MHz），dw_mshc 直接用作 `bus_hz`。曾把 SCMI 消息头位段记反（`message_id`/`token`），VERSION 能通但 RATE_GET 报 `PROTOCOL_ERROR`、被误判成"固件 stub"——实为 header 位段 bug（`message_id[7:0]`、`token[27:18]`），改正后 `ciu` 解析到真值。详见上文"RK3588 SCMI 实测"。

## 位段守护（harness）

SCMI 消息头的位段定义已进 [`data/bitfield-defs.json`](../data/bitfield-defs.json)（附 GENMASK 依据 + 已知错误布局 + 验证用例）。跑 `ldp check-bitfields` 会把每条验证用例在正确布局与错误布局下分别打包——**VERSION（msg=0/token=0）会被标为文档化 false-pass**（它无法区分正确/错误布局，正是当年骗人的根由）；若有人误标 VERSION "能区分"，check-bitfields 会报矛盾并退出码 1。新位段定义一律走这条流程，不要凭记忆写进代码/注释。
