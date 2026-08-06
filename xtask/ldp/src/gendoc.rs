// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ldp gen-doc`:从 data/*.json 重新生成 knowledge/*.md(数据为源)。
//!
//! 设计:
//! - **表格部分**由 JSON 生成,是真相源(改 JSON → 重生成)。
//! - **参考块**(源码 trait 签名等,非结构化数据)用 `GEN-DOC:APPENDIX` 标记保护,
//!   gen-doc 从旧文件抽出、原样回填,不会被覆盖。
//!
//! 生成后的 md 顶部带 banner,明确「本文件是构建产物,勿手改」。

use std::path::PathBuf;

use crate::data::{ApiMapping, Gaps};
use crate::repo;

const APPENDIX_START: &str = "<!-- GEN-DOC:APPENDIX-START (受保护参考块,gen-doc 原样保留) -->";
const APPENDIX_END: &str = "<!-- GEN-DOC:APPENDIX-END -->";

fn banner(src: &str) -> String {
    format!(
        "<!-- 本文件由 `ldp gen-doc` 从 {src} 生成。勿手改——改 {src} 后重跑 `ldp gen-doc`。 -->\n"
    )
}

/// 从旧文件抽取受保护的附录块;没有就返回空。
fn extract_appendix(path: &std::path::Path) -> String {
    let Ok(old) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let Some(s) = old.find(APPENDIX_START) else {
        return String::new();
    };
    let Some(e) = old.find(APPENDIX_END) else {
        return String::new();
    };
    old[s..e + APPENDIX_END.len()].to_string()
}

fn write_if_changed(path: &PathBuf, content: &str) -> bool {
    if std::fs::read_to_string(path).unwrap_or_default() == content {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, content);
    true
}

/// 生成 knowledge/api-mapping.md。
pub fn gen_api_mapping(api: &ApiMapping) {
    let mut sections = api.sections.clone();
    sections.sort_by_key(|s| s.order);
    let mut out = String::new();
    out.push_str(&banner("data/api-mapping.json"));
    out.push('\n');
    out.push_str("# Linux → x-kernel API 映射表\n\n");
    out.push_str("Phase 2(映射)的查表依据。**每一条 x-kernel 接口都来自项目源码**(括注源码位置)。");
    out.push_str("查表规则:命中 → 按「x-kernel 对应」替换;未命中 → 记缺口,转 `gaps.md`。");
    out.push_str("**不要自行编造相近接口(铁律 2)。**\n\n");
    out.push_str("**命令式查询**:跑 `ldp lookup <api>` 拿结构化候选(显式截断,不全文倾倒)。\n\n");
    out.push_str("符号约定:`dev` = `&DeviceObject`/`Arc<DeviceObject>`;");
    out.push_str(
        "`DriverResult<T> = Result<T, DriverError>`;`ResResult<T> = Result<T, ResError>`。\n\n",
    );

    for sec in &sections {
        out.push_str(&format!("## {}. {}\n\n", sec.order, sec.title));
        out.push_str("| Linux | x-kernel 对应 | 备注 / 位置 |\n");
        out.push_str("|-------|---------------|-------------|\n");
        for row in &sec.rows {
            let note_src = match &row.source {
                Some(src) => {
                    let line = src.line.map(|l| format!(":{l}")).unwrap_or_default();
                    if row.note.is_empty() {
                        format!("[{}{line}]", src.path)
                    } else {
                        format!("{} [{}{line}]", row.note, src.path)
                    }
                }
                None => row.note.clone(),
            };
            let badge = if row.status != "landed" {
                format!(" `{}`", row.status)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "| `{}`{} | {} | {} |\n",
                row.linux, badge, row.xkernel, note_src
            ));
        }
        out.push('\n');
    }

    out.push_str("## 缺失子系统速查(详见 `gaps.md`)\n\n");
    out.push_str("以下 Linux 子系统在 x-kernel 当前无对应实现,转写时几乎必撞,**一律按缺口上报**,不得静默 stub。\n");
    out.push_str(
        "> 完整清单、阻塞程度、替代方案、报告口径见 `gaps.md`(同样由 `ldp gen-doc` 生成)。\n\n",
    );

    out.push_str("---\n\n");
    out.push_str("## 参考(源码引用,gen-doc 原样保留)\n\n");
    out.push_str(
        "以下为源码 trait 签名等**参考信息**,非结构化数据,由 gen-doc 的受保护附录块承载。\n",
    );
    out.push_str("若要新增参考,编辑本文件 `GEN-DOC:APPENDIX` 标记之间的内容(不会被覆盖)。\n\n");
    let target = repo::skill_dir().join("knowledge/api-mapping.md");
    let appendix = extract_appendix(&target);
    if appendix.is_empty() {
        out.push_str(APPENDIX_START);
        out.push('\n');
        out.push_str(DEFAULT_API_APPENDIX);
        out.push_str(APPENDIX_END);
        out.push('\n');
    } else {
        out.push_str(&appendix);
        out.push('\n');
    }
    if write_if_changed(&target, &out) {
        println!("生成 {}", target.display());
    } else {
        println!("已是最新 {}", target.display());
    }
}

/// 首次生成时种入的参考块(从旧版 api-mapping.md 抢救出的 trait 签名)。
const DEFAULT_API_APPENDIX: &str = r#####"
**`DeviceDriver` trait 全貌**(`probe_device` 之外都有默认实现):

```rust
pub trait DeviceDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn device_kind(&self) -> DeviceKind;            // Block/Char/Net/Display/Input/Vsock/Fs9p/Bus
    fn bus_types(&self) -> &'static [BusTypeId];    // PLATFORM / PCI
    fn matcher(&self) -> &dyn DeviceMatcher;        // 返回你的 FirmwareMatchSpec
    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()>;
    fn remove(&self, device: Arc<DeviceObject>) -> DriverResult<()> { Ok(()) }
    fn suspend(&self, device: Arc<DeviceObject>) -> DriverResult<()> { Ok(()) }
    fn resume(&self, device: Arc<DeviceObject>) -> DriverResult<()> { Ok(()) }
    fn shutdown(&self, device: Arc<DeviceObject>) {}
}
```

**两种 probe 取址模式**(按硬件层约定选):
- vaddr 模式(sdmmc,推荐):`iomap_first_mmio(dev,"name")` 拿 `(NonNull<u8>,size)`,`unsafe { XxxDriver::new(vaddr.as_ptr() as usize) }`。
- paddr 模式(fxmac):`first_mmio_resource(dev)?` 拿 `MmioRegion.base`(物理),硬件层自管映射。

**中断处理闭包示例**(设备状态自引用时用 `Arc<Self>`):

```rust
struct RxIrq { dev: Arc<MyDev> }
impl IrqHandler for RxIrq {
    fn handle(&self) -> IrqEvent {
        // 读中断状态寄存器 → 清中断 → 处理
        IrqEvent::HANDLED
    }
}
```

**`BlockDevice` trait**(`drivers/block/src/lib.rs`,继承 `Device`):

```rust
pub trait BlockDevice: Device {
    fn num_blocks(&self) -> u64;
    fn block_size(&self) -> usize;
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult;
    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult;
    fn flush(&self) -> DriverResult;
}
```

**`Device` trait**(`driver_base`,所有设备基类):

```rust
pub trait Device: Send + Sync {
    fn name(&self) -> &str;
    fn device_kind(&self) -> DeviceKind;
    fn irq(&self) -> Option<usize> { None }
}
```

**错误集**:`DriverError` 全集 = `AlreadyExists / WouldBlock / BadState / InvalidInput / Io / NoMemory / ResourceBusy / Unsupported`;`ResError`(device-res)全集 = `InvalidResource / MappingFailed / NoMemory / Busy / Unsupported / NoProvider`。两套错误间用 `map_err` 转换。

**设备类 → publish_* 速查**:

| 硬件层 trait | publish 调用 | DeviceKind | 能挂的 RK3588 外设 |
|--------------|-------------|-----------|--------------------|
| `BlockDevice` | `kclass::publish_block` | `Block` | eMMC/SD (dwcmshc)、SATA (ahci) |
| `NetDevice` | `kclass::publish_net` | `Net` | 以太网 |
| `CharDevice` | `kclass::publish_char` | `Char` | UART、(降级)watchdog/rtc |
| —(无) | — | — | i2c/spi controller(**blocker**) |

**读时钟频率(MMC/I2C/UART/SPI 算分频都会撞)**:Linux 用 `clk_get_rate(ciu_clk)`;x-kernel 走 `clk::devm_clk_get(device,"ciu")`(总线已预解析 `ResourceDesc::Clock`,驱动不碰 DT)。**别想当然读 DT `clock-frequency`**——Rockchip MMC 节点通常只有 `max-frequency`(卡时钟上限/caps),**不是** CIU 输入速率,不能当分频分母。`devm_clk_get` 失败/速率 None 时按 fallback(对照 U-Boot `mmc info`)。详见 `clk-subsystem.md` 与 `gaps.md` clk 条目。

**HAL 抽象模式**:当硬件逻辑层 crate(如 `drivers/net/src/fxmac`)定义了自己的 `KernelFunc` trait 需要 v2p/p2v/dma_alloc/irq 时,胶水层用 `#[crate_interface::impl_interface]` 提供宿主实现。新驱动优先直接用 `device-res` 的 `devm_*`,避免再造 HAL trait。

**早期 init 的 `log::*` 是静默的**:`khal::early_driver_init()` 早于 "Logging is enabled"。
诊断改用 `kernel_boot::bootln!("...")`(boot 控制台,日志带 `[boot]` 前缀)。
"#####;

/// 首次生成时种入 gaps.md 的参考块（从原版 gaps.md 抢救的 bring-up 坑叙事）。
const DEFAULT_GAPS_APPENDIX: &str = r#####"
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
"#####;

/// 生成 knowledge/gaps.md。
pub fn gen_gaps(gaps: &Gaps) {
    let mut out = String::new();
    out.push_str(&banner("data/gaps.json"));
    out.push('\n');
    out.push_str("# 已知缺失子系统清单(Gap Catalog)\n\n");
    out.push_str("x-kernel 目前只实现了驱动模型的「主干」:MMIO / IRQ / coherent DMA / 同步原语 / 设备类发布。");
    out.push_str("Linux 驱动里大量依赖的子系统**这里没有**。\n\n");
    out.push_str("**命令式查询**:跑 `ldp gaps [--status <s>]` 或 `ldp gaps --id <id>`。\n\n");

    out.push_str("## 阻塞程度分级\n\n");
    out.push_str("| 级别 | 含义 | 处置 |\n|------|------|------|\n");
    out.push_str("| `blocker` | 没它驱动无法初始化/工作 | 必须先补,或明确接受「不可用」,**不得静默 stub** |\n");
    out.push_str("| `works-around` | 可绕过/stub,受限场景能跑 | 写明 stub 与前提,缺口照报 |\n");
    out.push_str("| `optional` | 锦上添花 | 缺口照报,低优先级 |\n\n");
    out.push_str("> `conditional` = 按使用维度分别定级(见各条 dimensions)。\n\n");

    out.push_str("## 逐项缺口\n\n");
    for g in &gaps.subsystems {
        out.push_str(&format!("### {} (`{}`)\n\n", g.name, g.id));
        out.push_str(&format!("- **状态**:`{}`\n", g.status));
        if !g.xkernel_status.is_empty() {
            out.push_str(&format!("- **现状**:{}\n", g.xkernel_status));
        }
        if !g.symbols.is_empty() {
            let syms = g
                .symbols
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("- **Linux 符号**:{syms}\n"));
        }
        if g.dimensions.is_empty() {
            out.push_str(&format!("- **阻塞程度**:`{}`\n", g.severity));
            if !g.workaround.is_empty() {
                out.push_str(&format!("- **替代**:{}\n", g.workaround));
            }
            if !g.report_template.is_empty() {
                out.push_str(&format!("- **报告口径**:{}\n", g.report_template));
            }
        } else {
            out.push_str("- **按维度定级**:\n");
            for d in &g.dimensions {
                out.push_str(&format!(
                    "  - `{}` ({}): {}",
                    d.name, d.severity, d.workaround
                ));
                if !d.report_template.is_empty() {
                    out.push_str(&format!("  · 口径:{}", d.report_template));
                }
                out.push('\n');
            }
        }
        out.push('\n');
    }

    if !gaps.cascade_gaps.is_empty() {
        out.push_str("## 级联缺口(常见组合)\n\n");
        out.push_str("| 区域 | 缺口链 |\n|------|--------|\n");
        for c in &gaps.cascade_gaps {
            let area = c.get("area").and_then(|v| v.as_str()).unwrap_or("?");
            let chain = c
                .get("chain")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" + ")
                })
                .unwrap_or_default();
            out.push_str(&format!("| {area} | {chain} |\n"));
        }
        out.push('\n');
    }

    out.push_str("---\n\n");
    out.push_str("> 移植前先看 `examples/dwcmshc-walkthrough.md` 里 sdmmc 如何处理 clk/phy 缺口——它是 works-around 的真实范例。\n");

    out.push_str("\n## 参考：真机 bring-up 易踩的坑（gen-doc 原样保留）\n\n");
    out.push_str("以下为**硬件现实**类叙事（非子系统缺口，但会让“编译通过”的驱动在硬件上挂掉），由 gen-doc 的受保护附录块承载。\n若要增删，编辑本文件 `GEN-DOC:APPENDIX` 标记之间的内容。\n\n");
    let target = repo::skill_dir().join("knowledge/gaps.md");
    let appendix = extract_appendix(&target);
    if appendix.is_empty() {
        out.push_str(APPENDIX_START);
        out.push('\n');
        out.push_str(DEFAULT_GAPS_APPENDIX);
        out.push_str(APPENDIX_END);
        out.push('\n');
    } else {
        out.push_str(&appendix);
        out.push('\n');
    }
    if write_if_changed(&target, &out) {
        println!("生成 {}", target.display());
    } else {
        println!("已是最新 {}", target.display());
    }
}
