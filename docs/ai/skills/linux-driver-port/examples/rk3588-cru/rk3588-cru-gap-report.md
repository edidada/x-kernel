# 移植缺口报告 — RK3588 CRU (clk-rk3588.c → x-kernel)

| 字段 | 值 |
|------|----|
| Linux 源 | `drivers/clk/rockchip/clk-rk3588.c`(+ clk.c/clk.h/clk-pll.c/clk-cpu.c/clk-half-divider.c/gate-link.c),参考树 `linux`(linux-7.1.8 快照)|
| DT 绑定 id 参考 | `bsp-opi5plus`(orange-pi-5.10-rk3588 @ eb1c681e5184,与 orangepi-5-plus.dts 逐项核对:SCLK_UART2=187、CCLK_EMMC=314、SRST_C_EMMC=502、assigned-clocks 0x9=PLL_PPLL=1.1GHz 等)|
| 目标 compatible | `rockchip,rk3588-cru` |
| 目标 DeviceKind / bus | `DeviceKind::Bus` / PLATFORM |
| x-kernel 落点 | 胶水+硬件逻辑:`drivers/integration/kdriver/src/driver_registry/clk/rk3588_cru/{mod,ops,data}.rs`;框架:`core/kclk`、`device-res`、`kdevice`、`khal`、`platform_backend` |
| 需求 issue | `req-clk-framework`(confirmed → 本报告后标 landed)、`req-busy-wait`(confirmed:用 `khal::time::busy_wait` 现有原语)|
| 移植结论 | **受限可用(真机验证点全开)**:CRU 全树注册 + 消费侧解析/改频闭环;SCMI 相位钟、复位控制器、重启通知未移植(见阻塞项) |

## 依赖的 Linux 子系统清单

- ✅ 时钟框架(核心路径)— `req-clk-framework`
- ✅ 忙等延时(PLL 锁等待)— `khal::time::busy_wait`
- ❌ regmap(rockchip,grf)— RK3588 PLL 锁状态走 PLLCON6,GRF 实际未用,无需替代
- ❌ runtime PM / pm_clk(gate-link 的 linked clock 持链)— 见逐条表
- ❌ SCMI clock provider(scmi_cclk_sd / scmi_sclk_sd)— 见逐条表
- ❌ reset controller(rst-rk3588.c)/ restart notifier — 见逐条表

## 逐条缺口

| 子系统 | 严重度 | Linux 用法 | x-kernel 处置 | 状态 |
|--------|--------|-----------|---------------|------|
| clk-framework(核心) | conditional(provider_unavailable=blocker) | clk_hw_register_composite/mux/divider/gate、of_clk_add_provider、rate table PLL/cpuclk/frac | core/kclk 框架(已落地)+ F1-F8 框架增量(下表)+ rk3588_cru 驱动(PLL×9、armclk×3、静态分支表 648 项、frac×31、halfdiv×3、gate-link×22) | **完成** |
| frac 分支(31) | — | clk_fractional_divider_ops + rockchip approximation + 子 mux remux notifier(clk.c:140-175) | `FracBranch`(frac div+gate)+ 普通 Mux 子钟 + PRE/POST notifier 舞步(PRE 切到 frac 位,POST/ABORT 切回;abort 时恢复是对 Linux 的安全加固) | **完成** |
| half divider(3) | — | clk_half_divider_recalc/set(bestdiv) | `HalfDivComposite`(mux+halfdiv+gate 复合 ops) | **完成** |
| cpuclk(3) | — | rockchip_clk_register_armclk:单父+SET_RATE_PARENT+NO_REPARENT、alt 父常使能、父钟 PRE/POST notifier 安全切换 | `ArmClk`(recalc=prate/(div+1))+ alt 父 prepare_enable + 主父 notifier 舞步(①alt 分频器 ②pre_muxs ③mux→alt;POST 反序) | **完成** |
| gate-link(22) | works-around | gate + 平台设备 pm_clk 持 linked clock(runtime PM) | gate 照常注册 + probe 持 linked clock `prepare_enable()` 终身(boot-static;无 runtime PM 的等价语义) | **完成(适配)** |
| MMC 相位(4) | conditional | MMC(SCLK_SDMMC_DRV/SAMPLE,父=scmi_cclk_sd/scmi_sclk_sd) | **不移植**:父钟属于 SCMI 固件 provider,本仓库无 SCMI clk provider;相位调优无消费方。DT 里 sdmmc 的 ciu-drive/sample(<&cru 706/707>)解析将 NotFound(诚实错误) | **缺口照报** |
| reset controller | conditional | rst-rk3588.c(508 复位线,DT #reset-cells=1;eMMC 节点引用 SRST_500-504) | **不移植**:无 reset 框架(gaps.json `reset` conditional)。eMMC/dwcmshc 驱动落地时需先建 reset provider | **缺口照报** |
| restart notifier | optional | rockchip_register_restart_notifier(GLB_SRST_FST=0xc08 写 0xgada7),reboot 用 | **不移植**:内核暂无重启钩子 | **缺口照报** |
| clk_set_rate_no_reparent | works-around | 4×dclk_vop*(display 像素钟)带 NO_REPARENT | 框架已加 `ClkFlags::NO_REPARENT`(F8),determine_rate 只见当前父 | **完成** |

## 框架增量(需求 req-clk-framework 的验收项)

| # | 增量 | 位置 |
|---|------|------|
| F1 | register_table 父名全局回退(镜像 `__clk_lookup`),仍解析不到报错回滚 | kclk/table.rs + provider.rs `clk_get_core_by_name` |
| F2 | `ClkProviderDesc` 支持多输出控制器(`fixed_rate_hz: Option` + `controller(id)`) | device-res/clk.rs、platform_backend.rs、clk-fixed-clock 驱动适配 |
| F3 | assigned-clocks/assigned-clock-rates 枚举为 `ResourceDesc::AssignedClock` | khal/firmware/devices.rs、of/clocks.rs(1/2 cell 双格式)、platform_backend、kdevice `assigned_clocks()` |
| F5 | `MuxFlags::HIWORD_MASK`、`DividerFlags::HIWORD_MASK/READ_ONLY`(单次写消除 RMW 丢更新窗口) | kclk/basic/{mux,divider}.rs |
| F6 | SET_RATE_PARENT 传播时对父节点也发 PRE/POST(失败 ABORTED)notifier | kclk/core.rs `program_rate_locked` |
| F7 | 同速率 set_rate 短路(不 notify、不写硬件;U-Boot 已配好的 assigned 值安全) | kclk/core.rs `set_rate` |
| F8 | `ClkFlags::NO_REPARENT`(determine_rate 只见当前父;PLL mode mux 与 dclk_vop 依赖) | kclk/flags.rs + core.rs |

## 外部输入钟占位(11+11=22,Linux 孤儿等价)

`data.rs ORPHAN_PARENTS`:i2s0-10_mclkin(pinctrl 输入 ×11)、clk_hdmiphy_pixel0/1、
clk_gmac0/1_ptpref_io、clk_pipe30phy_pipe0/2_i、clk_pipephy0/1/2_pipe_i、aclk_vop_div2_src、
dummy_aupll。驱动注册 rate=0 占位(名字与参考一致);后续 PHY/GMAC/VOP 驱动落地时以真
provider 接管(先到先得)。**孤儿本身不放行**:kclk 事务注册对解析不到的父名仍报错回滚。

## id 域合成索引(报告承诺)

- BSP 头缺失、mainline 独有的钟(22 项,gate-link 家族为主):索引取合成区间
  722+(BSP 头内部 228 重复(TCLK_WDT_DDR/CLK_DDR_CM0_RTC 冲突)时后者合成)。
- 名字填充钟(Linux 表内 id=0:sclk_dsu、xin12m 等 15 项):合成区间 745-759。
- 以上 DT 均无法按索引引用,只影响 by-name 查找;不影响真机消费。

## 阻塞项汇总(决策门)

- 决策门 `ldp scan <references/linux/drivers/clk/rockchip> --gate`:**通过**(exit 0)。
- 剩余可阻塞分支 `clk-rate.provider_unavailable` 由本移植消解(CRU provider 落地)。
- SCMI 相位钟、reset、restart 保持缺口,均不影响「CRU 注册 + 消费解析」路径。

## 可编译性结论(P5)

- `cp platforms/kplat-aarch64/rk3588_defconfig .config && make defconfig && make build`:**通过**
- `make clippy`:**本移植代码 0 error/0 warning**(仓库预存 knet `send_poller` dead-code
  在 HEAD 上即失败,与本移植无关,已用 `git stash` 复核)。
- 注册三连:kdriver Cargo feature `clk-rk3588-cru` → api/kfeat `driver_clk_rk3588_cru`
  → Kconfig `KFEAT_DRIVER_CLK_RK3588_CRU` → rk3588_defconfig 已设 y。

## 集成状态(P5)

- firmware_specs:CRU 匹配走 `FirmwareMatchSpec{dt_compatibles:["rockchip,rk3588-cru"]}`(mod.rs)
- clk/mod.rs `register_rk3588_cru` 已挂入 `register_platform_drivers`
- DT:clock-controller@fd7c0000(0xfd7c0000+0x5c000,phandle 0x02)由 PlatformBackend
  以 provider-first 顺序枚举;消费端 UART(187/172)、eMMC(312-316)随即可解析。

## 真机验证(P6,Orange Pi 5 Plus)

步骤(串口 1500000 8N1,USB-TTL 于调试口):

```bash
cp platforms/kplat-aarch64/rk3588_defconfig .config && make defconfig
make build
# U-Boot: booti <kernel Image> - <rk3588-orangepi-5-plus.dtb>
```

验收点(日志逐条可查):

1. `clk-rk3588: provider 0x2 ready; gpll=1188000000Hz ...` — gpll 1188MHz、cpll
   786.432MHz 与 BSP 期望一致;sclk_uart2 非零且等于 24MHz 源分频值。
2. `serial: programming baud from <rate> Hz clock` — ns16550 控制台用 CRU baudclk
   (id 187)重编程分频,之后内核日志继续以 1.5 Mbaud 正常输出(闭环)。
3. assigned-clocks 日志无 `unresolved`/`assigned rate ... :` 警告(19 组中
   PLL_PPLL=1.1G、PLL_AUPLL=786.432M、PLL_NPLL=850M、PLL_GPLL=1188M 同值短路,
   不触碰硬件)。
4. `clk_summary` 抽查(gpll/cpll/clk_uart2/clk_1000m_src)与 U-Boot `clk dump` 对照。

未跑通 P6 的维度照旧标注「未验证」;SCMI 相位钟、reset、restart 保持「不可用」。
