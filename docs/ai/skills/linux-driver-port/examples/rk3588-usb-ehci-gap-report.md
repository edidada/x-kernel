<!-- 本文件由 `ldp scan --report` 自动生成(草稿)。
人工补全:compatible / DeviceKind / 落点 / 逐条处置 / 可编译性结论 / 集成状态。
模板见 templates/gap-report.md。 -->

# 移植缺口报告(草稿)— ../docs/ai/skills/linux-driver-port/data/references/linux/drivers/usb/host/ehci-hcd.c

| 字段 | 值 |
|------|----|
| Linux 源 | `drivers/usb/host/ehci-hcd.c`(+ehci-hub/ehci-sched;generic-ehci 平台端;ohci 同批)|
| 目标 compatible | `rockchip,rk3588-ehci`/`generic-ehci`(fc800000、fc880000)+ `generic-ohci`(fc840000、fc8c0000)|
| 目标 类/栈 | 新 `drivers/subsystems/usb`(usb core 最小面)+ HCD 驱动(PlatformBus)|
| x-kernel 落点 | `drivers/subsystems/usb/`(core)+ `drivers/drv/usb/src/{ehci,ohci,usb2phy}.rs` |
| 移植结论 | **受限可用路径已定**:host-only、全 coherent DMA、INTx、usb2phy(仅 init/power_on);usb3/dwc3 与 companion 细节后置 |

## 依赖的 Linux 子系统清单(scan 自动命中)

- [ ] ❌ `completion` [workaround] 命中: complete
- [ ] ✅ `忙等延时(udelay/mdelay/ndelay)` [landed] 命中: udelay
- [ ] ✅ `定时器(timer_setup/mod_timer/hrtimer)` [landed] 命中: timer_setup, mod_timer, hrtimer
- [ ] ❌ `设备子系统无 class(i2c/spi/watchdog/regulator/usb)` [missing] 命中: usb_hcd
- [ ] ❌ `其他(vmalloc/RCU/kmem_cache/leds/pm_runtime)` [workaround] 命中: vmalloc
- [ ] ❌ `USB 主机栈(usb core + HCD)` [missing] 命中: ehci_setup
- [ ] ❌ `DMA streaming 映射` [missing] 命中: dma_map_single

## 逐条缺口(处置 2026-07)

| 子系统 | 严重度模型 | 现状 | 处置 | 需求 issue |
|--------|-----------|------|------|-----------|
| **USB 主机栈(usb core + HCD)** | conditional | gaps `usb-stack` [missing] | 本切片本体:subsystems/usb 最小面 + EHCI/OHCI HCD | req-usb-stack **in-progress** |
| **DMA streaming** | conditional | 只有 coherent 分配 | 首版全 coherent(qh/qtd/ed/td + 缓冲) | req-dma-streaming **rejected** |
| **phy**(usb2phy) | independent_phy_ip=blocker | gaps `phy` [missing] | 与 PCIe 共用 req-phy provider 框架;usb2phy 为第三后端(仅 init/power_on) | req-phy **in-progress** |
| **power-domains** | conditional | 无 PD 框架(新增缺口条目) | 首版假定固件已使能 PD,probe 日志标注;真实 PD 开关需求出现再立项 | (gaps.json `power-domain` 新增,open) |
| **completion**(HCD 完成事件) | works-around | WaitQueue 替代 | ktask::WaitQueue | req-completion **confirmed** |
| **定时器**(ehci watchdog) | landed | ktimer-core + kworkng | Timer + DelayedWorkHandle | req-timer **landed** |
| **vmalloc**(dbg buffer) | optional | 无 | 不移植(仅 debugfs 路径) | req-misc **rejected** |

## 阻塞项汇总(决策门,生成代码之前必须过)

- ⚠ `device-class`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `usb-stack`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `dma-streaming`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。

## 可编译性结论

- 待 Phase 5(P3 代码生成后):`make build`(rk3588_defconfig)+ `make clippy`。

## 移植顺序(解 blocker 的拓扑)

1. req-reset + req-phy(与 PCIe 共享的框架,先落)。
2. `subsystems/usb` usb core 最小面(hub/地址/描述符)+ class 契约。
3. EHCI(qh/qtd coherent ring)→ OHCI(ed/td)→ usb2phy 后端。
4. dwc3/xHCI(USB3)与 usbdp 后置。

## 集成状态

- [x] P0-P2 完成(本报告 + portrait + 需求决策);P3 待下一编码段
> 在草稿上继续补全(处置、编号参考序列对照表、集成记录),完成后即正式缺口报告。
