# SD/MMC 线现状(实测,截至 2026-10-26)

> 对象:`xiaohui/driver-test` 分支(worktree
> `xiaohui-driver-model-v2-refactor-worktrees/pr1`),基线 `db76e8a94`
> (= `xiaohui/driver-model-v2-refactor` 头),其上 41 个 MMC commit。
> 本文修正 `difficulty-assessment.md` 中"MMC 全缺"的判断——那是 PR 1081
> 分支的事实;这条活跃线上 MMC 已成形。

## 1. 已落地的代码(约 3.5 K 行,全部带 design/security 文档)

| 位置 | 内容 | Linux 对应 |
|---|---|---|
| `drivers/subsystems/mmc` | `MmcHost` 契约、mmc 卡总线(卡是域设备,挂 host 之下)、SD 检测状态机(CMD8/ACMD41/CID/CSD/SCR/ACMD6/CMD6-HS,按协议阶段分三段)、`blk.rs`→Gendisk 分区发布、`XferSignal` 双半完成设计 | `drivers/mmc/core/*` |
| `drivers/subsystems/mmc-host` | 任务上下文等待半边(probe 期退化为有界轮询) | completion |
| `drivers/drv/mmc/dw_mmc.rs` | DW MSHC 寄存器面、分频数学(dw_mci_setup_bus 形)、**FIFO 轮询 PIO 数据路径**(板定形态)、databook 级解楔序列(ABORT_READ_DATA 脉冲→DAT0 释放→FIFO reset→CMD12)、HCON.HDATA_WIDTH 决定 FIFO 访问宽度与 FCNT 单位 | `dw_mmc.c` |
| `drivers/drv/mmc/dw_mmc_rockchip.rs` | platform probe;**CIU 速率=板件事实 `MMC_CIU_HZ`**(50 MHz),不再走 SCMI/Secure CRU(均已实测不可用,结论写在代码注释) | `dw_mmc-rockchip.c` |
| `drivers/drv/clk/scmi_clock.rs` | SCMI clock provider(464 行,8 个 commit 的 SMC/shmem 协议考古;**负结果**:本板 bl31 不服务主线 SCMI clock 协议) | firmware clock |
| `fs/boot` | 分区扫描归块消费侧,mmcblk0pN 一盘一分区,root 挂载路径 | |

已修复并留档的坑:CMD 索引 6 位截断(0x1F 掩码把 ACMD41 写成 CMD9)、
per-transfer FIFOTH、陈旧 RINTSTS 语义、phase 边界恢复退化代价
(STATUS.DATA_STATE_MC_BUSY 悬挂→假 DATA_OVER)。

## 2. 当前板级状态(debug.log,10-26)

```
scmi-clk: msg 0x3 failed: status 9        ← CLOCK_ATTRIBUTES,bl31 拒答
scmi-clock gave up on device 71/392/447   ← provider 不注册
dwmmc-rockchip deferred device 310 on clk:14   ← 0x0e = SCMI clock 节点 phandle
dwmmc-rockchip deferred device 311 on clk:2    ← 0x02 = CRU phandle
fs_boot: "No block device found!" panic
```

**阻塞点不在驱动,在框架依赖**:设备枚举时 `firmware/of/clocks.rs` 把 DT
`clocks` 属性转成 `ResourceKind::Clk` 依赖,SD 节点的四条 clock spec
(`biu/ciu`←SCMI phandle 0x0e、`ciu-drive/ciu-sample`←CRU phandle 0x02)
都没有 provider → 设备永久 defer。驱动自身已不调 `clk()`(板件事实路线),
但框架层仍挡在 probe 之前。

## 3. 两个待验证项(代码已写,板未复测)

1. `d1ffbee44`(databook 解楔 + FIFO 宽度 + 降速旋钮)提交于 10-26 01:59,
   debug.log 早于它的板级复测——512B 数据相位的楔死是否已解,**未证**。
2. IDMAC→PIO 的路线反复过一次(曾"verified tgoskits semantics"后移除);
   若 PIO 楔死仍在,内置 **SDMA 单描述符模式**是第三条路(Linux 同样支持,
   比 IDMAC 链和 PIO 轮询都简单)。
