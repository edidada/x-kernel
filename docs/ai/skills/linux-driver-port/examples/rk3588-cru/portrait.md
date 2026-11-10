# RK3588 CRU 移植画像(P0/P1)— clk-rk3588.c → x-kernel

## P0 采集 — 移植任务卡

| 字段 | 值 |
|------|----|
| Linux 源 | `drivers/clk/rockchip/clk-rk3588.c`(参考树 `linux` = /workspace/linux-7.1.8 快照,已 vendor)|
| 依赖的 rockchip 基础设施 | `clk.c` `clk.h` `clk-pll.c` `clk-cpu.c` `clk-half-divider.c` `gate-link.c` `rst-rk3588.c`(同一参考树)|
| compatible | `rockchip,rk3588-cru` |
| DeviceKind / bus | `DeviceKind::Bus`(基础设施设备,同 `clk-fixed-clock` 先例)/ PLATFORM |
| x-kernel 落点 | 驱动 crate:`drivers/drv/clk/src/rk3588_cru/`(mod.rs 绑定+probe、data.rs 生成表、ops.rs 自定义 PLL/armclk/frac/halfdiv ops)|
| 是否已存在 | 否(本仓库仅有 fixed-clock/fixed-factor 通用驱动;`core/kclk` 框架已落地)|
| DT(真机) | `orangepi-5-plus.dts`:`clock-controller@fd7c0000` = `rockchip,rk3588-cru`,reg 0xfd7c0000+0x5c000,`#clock-cells=<1>`,`phandle=<0x02>`,19 组 assigned-clocks/rates |
| **DT 绑定 id 编号** | **BSP 编号,非 mainline**。OPi5+ DTB 按 Rockchip 5.10 BSP 头编译(SCLK_UART2=187、CCLK_EMMC=314、SCLK_SDMMC_DRV=706;mainline 7.1.8 头分别是 175/300/685)。→ 增设参考树 `bsp-opi5plus`(orange-pi-5.10-rk3588 @ eb1c681e5184),**注册索引取 BSP 头,寄存器/位段/时序取 linux 7.1.8**。两参考各自钉死、各管一个数据域。|

### 真机验证路径(P6 候选)

- 控制台 UART2(0xfeb50000,ns16550-mmio):DT `clocks = <&cru 187> <&cru 172>`(baudclk/apb_pclk)。
  `serial` 驱动已实现 `devm_clk_get → get_rate → 按实际时钟编程分频`(日志 `serial: programming baud from {rate} Hz clock`)。
  CRU 注册后:baudclk=sclk_uart2 解析到真值(24MHz 源经 mux/frac/div)→ 控制台继续以 1.5 Mbaud 正常输出 = 消费侧闭环证据。
- `clk_summary` 关键钟对照:U-Boot 已配置的 gpll/cpll/lpll/... 与 assigned-clock-rates 一致。
- assigned-clocks 复用(见 P3.4):U-Boot 已配好的钟 set_rate 同值短路,不改硬件;不同值的钟走真实 set_rate。

## P1 分析 — 驱动画像

### 1. probe 流程(Linux 两段,本移植合为一次 platform probe)

```text
CLK_OF_DECLARE(rk3588_cru) 早期段:                      platform probe 段:
  of_iomap(reg 0)                                         rockchip_clk_register_late_branches(22×GATE_LINK)
  rockchip_clk_init_early(np, base, nr_clks)              rockchip_clk_finalize
  rockchip_clk_register_plls(9 PLL, GRF_SOC_STATUS0)      rk3588_rst_init(np, reg_base)
  rockchip_clk_register_armclk ×3 (armclk_l/b01/b23)      rockchip_register_restart_notifier(GLB_SRST_FST)
  rockchip_clk_register_branches(early_branches)          of_clk_del_provider + re-add(重排默认值)
  rockchip_clk_of_add_provider(np, ctx)
```

x-kernel 对应(单次 probe,`DriverOps<PlatformBus>`):`dev.res().iomap(0)` → register PLL(9)→ armclk(3)→ early branches → late branches(GATE_LINK)→ resets/restart(缺口,见 P4)→ `kclk::register_table` + `register_provider` → assigned-clocks 应用(DT 19 组,经 `assigned-clocks`/`assigned-clock-rates` 属性读取;Linux 由驱动核在 probe 时应用)。

### 2. 依赖的子系统(API → 缺口候选)

| Linux API | 用途 | x-kernel 对应 |
|-----------|------|---------------|
| `of_iomap` / `iounmap` | CRU 寄存器窗口 0xfd7c0000+0x5c000 | `dev.res().iomap(0)`(IoMap guard;boot-static)|
| `clk_hw_register_composite/mux/divider/gate` | 分支注册 | `kclk::register_table`(ClkBranch 静态表)|
| `clk_fractional_divider_ops` | 31×frac 分频 | 驱动内 Custom ops(`FracDiv`),kclk 无 frac 基本类型 |
| `rockchip_clk_register_halfdiv` | 3×半分频 composite | 驱动内 Custom ops(`HalfDivComposite`)|
| `rockchip_clk_register_armclk` | 3×CPU clk | 驱动内 Custom ops(`ArmClk`)+ 父 PLL notifier |
| `rockchip pll ops (rk3588/rk3588_core)` | 9×PLL | 驱动内 Custom ops(`Rk3588Pll`)|
| `clk_register_gate` + `pm_clk`(gate-link) | 22×GATE_LINK | `ClkBranch::gate` + probe 内持有 linked clock 常使能(无 runtime PM 的等价适配)|
| `rockchip_clk_register_mmc` | 4×MMC 相位(父=scmi_cclk/sclk_sd) | **不移植**(父钟来自 SCMI 固件,本仓库无 SCMI clk provider;见 P4)|
| `rst-rk3588.c` / `rockchip_register_restart_notifier` | 复位控制器 + 重启 | **不移植**(无 reset 框架/重启钩子;见 P4)|
| `syscon_regmap_lookup_by_phandle(rockchip,grf)` | GRF(rk3588 PLL 锁状态实际走 PLLCON6,GRF 未用) | 不需要 |
| `udelay` | PLL 锁等待(1000µs 上限轮询) | 忙等轮询读 PLLCON6(核内 wait_lock,间隙 ~寄存器读周期)|

### 3. 寄存器初始化序列(编号参考清单 — 铁律 1)

真相源:linux 7.1.8 `clk-pll.c` rk3588 段 + `clk-cpu.c` + `clk.c`。逐条编号(生成代码必须逐条对应):

**PLL set_params(rk3588 类型,含 v0pll/aupll/cpll/gpll/npll;pll_rk3588_core 类型无 ①⑦ 两步):**

```text
① 若 mode_mux 当前 == NORM(1):写 mode_reg = HIWORD_UPDATE(SLOW=0, mask=0x3, mode_shift)   // clk-pll.c:946-950
② 写 PLL_CON1 = HIWORD_UPDATE(PWRDOWN(bit13), bit13, 0)          // 掉电,clk-pll.c:955
③ 写 PLL_CON0 = HIWORD_UPDATE(m, 0x3ff, 0)                       // clk-pll.c:960
④ 写 PLL_CON1 = HIWORD_UPDATE(p, 0x3f, 0) | HIWORD_UPDATE(s, 0x7, 6)   // clk-pll.c:963
⑤ 写 PLL_CON2 = HIWORD_UPDATE(k, 0xffff, 0)                      // clk-pll.c:967
⑥ 写 PLL_CON1 = HIWORD_UPDATE(0, PWRDOWN, 0)                     // 上电,clk-pll.c:971
⑦ 轮询 PLL_CON6.bit15 (LOCK_STATUS) 直到置位,上限 1000µs        // clk-pll.c:877-882
⑧ 超时 → 递归用旧参数恢复(①-⑦),打 warn                         // clk-pll.c:975-979
⑨ (rk3588 类型且①发生)写 mode_reg = HIWORD_UPDATE(NORM=1, 0x3, shift)  // clk-pll.c:982
```

HIWORD_UPDATE(val,mask,shift) = `(val << shift) | (mask << (shift+16))`(clk.h:26)。写 32 位单次(hiword 是写使能掩码,读回高 16 位为 0)。

**PLL recalc_rate(clk-pll.c:903-920):** `rate = prate*m/p;若 k:rate += prate*k/(p*65535);rate >>= s`。

**PLL 注册形态(clk-pll.c:1036-1100):** 每只 PLL 产生两个钟:实际 PLL `pll_<name>`(父=parent_names[0]=xin24m,ops=rk3588 pll)和 mode mux `<name>`(父=[xin24m, pll_<name>, xin32k],mask=0x3,HIWORD,CLK_SET_RATE_PARENT)。

**armclk(clk-cpu.c):** 单钟、单有效父=mux_core_main 对应的 PLL;ops.recalc=prate/(div_core(raw)+1)(clk-cpu.c:60-70);flags=SET_RATE_PARENT|NO_REPARENT|RATE_NOCACHE;alt 父(xin24m)常 prepare+enable(clk-cpu.c:40-46);在主父(PLL)上挂 PRE/POST notifier(clk-cpu.c:57-62):

```text
PRE(parent rate 即将变):                                        // clk-cpu.c:152-217
 ① 查表取 ndata->new_rate 的 settings(无表项 → -EINVAL)
 ② 若 alt_prate(xin24m=24M) > old_rate:全部 core 分频器写 alt_div=DIV_ROUND_UP(alt_prate,old)-1
 ③ 写 pre_muxs[](每组 {reg,val} 原样直写)
 ④ 写 mux = HIWORD_UPDATE(mux_core_alt=1, mask, shift)(mux_core_reg 或 core_reg[0])
POST(父 rate 已变):                                             // clk-cpu.c:223-266
 ① 若 old < new:写 post_muxs + 全部 core 分频器清 0 + (无)
 ② 写 mux = HIWORD_UPDATE(mux_core_main=2, mask, shift)
 ③ 写 post_muxs[](每组 {reg,val} 原样直写)
 ④ 全部 core 分频器写 0(HIWORD_UPDATE(0,mask,shift))
 ⑤ 若 old > new:写 post_muxs
```

**frac 分支(clk.c:201-290 + clk-fractional-divider.c):** composite(无 mux、frac div、gate,flags|SET_RATE_UNGATE);div: mshift=16/mwidth=16(分子),nshift=0/nwidth=16(分母);recalc=m/n 为 0 → 直通父,否则 prate*m/n(clk-fd.c:recalc);set=rational_best_approximation(rate,prate,0xffff,0xffff) 后单次写 m|n(clk-fd.c:set_rate,mask 合并字段,HIWORD 无);rockchip approximation(clk.c):若 `rate*20 > p_rate 且 p_rate%rate != 0` → 用**祖父** rate 作等效父;再走 general(GCD 缩放)逼近。子 mux 单独注册(CF 上的 mux),frac 挂 PRE/POST notifier:PRE 把 mux 切到 frac 位,POST 切回原位(clk.c:140-175)。

**half divider(clk-half-divider.c):** recalc=DIV_ROUND_UP(prate*2, raw*2+3);set: value=(DIV_ROUND_UP(prate*2,rate)-3)/2,HIWORD 写;round:遍历 raw 找不超请求的最大值(无 ROUND_CLOSEST)。

**分支注册旗标(clk.c:497-675):** branch_gate/branch_grf_gate → flags |= CLK_SET_RATE_PARENT;composite 按有无 mux/div/gate 组装;GFLAGS = HIWORD|SET_TO_DISABLE(高有效位=关);MFLAGS = HIWORD;DFLAGS = HIWORD(8 处另有 READ_ONLY)。

**GATE_LINK(gate-link.c + clk.c:478-505):** gate 本体 = clk_register_gate(flags|SET_RATE_PARENT);link 语义 = 平台设备持有 linked clock 的 pm 引用(runtime PM)。x-kernel 等价:probe 时对该 linked clock 取 handle 并常使能(boot-static,无 runtime PM)。

### 4. 时钟表规模(7.1.8 clk-rk3588.c,机械提取)

PLL×9、armclk×3、GATE×377、COMPOSITE×137、COMPOSITE_NODIV×82、COMPOSITE_NOMUX×20、COMPOSITE_HALFDIV×3、COMPOSITE_FRACMUX×31、MUX×91、DIV×85、FACTOR×1、GATE_LINK×22、MMC×4(不移植)。生成物:`data.rs` 静态表 + 外部父钟占位(见 P2 决策)。

### 5. 中断/私有数据/设备类操作

无中断(轮询锁状态);私有状态 = devm MMIO 窗口 + kclk::ClkTable(phandle 0x02)+ 9 PLL / 3 armclk 的 Custom ops 对象;无设备类发布(基础设施驱动,不进 kclass)。

### 6. 外部父钟(表内解析不到的名字)

- DT fixed-clock:`xin24m`(phandle 0x131)、`xin32k`(0x130)、`spll`(0x1f5)→ 走 kclk 跨 provider 注册表(全局名回退,需求 req-clk-...-F1)。
- 无 provider 的外部输入:`i2s{0..10}_mclkin`×13(pinctrl 输入)、`clk_hdmiphy_pixel{0,1}`(HDMI PHY)、`clk_gmac{0,1}_ptpref_io`(GMAC)、`dummy_aupll`(未绑定 pad)→ 驱动注册 **0 速率占位钟**(Linux 孤儿等价:rate 0,名字与参考一致,先到先得、后续真 provider 落地后接管),逐个列名记入缺口报告。
- SCMI:`scmi_cclk_sd`/`scmi_sclk_sd` → 4 条 MMC 相位项整体不移植。

### 7. id 索引域

注册索引 = BSP 头(参考树 `bsp-opi5plus`)的 id,使 `clocks = <0x02 N>` 消费解析与 DT 一致;表项与寄存器域 = linux 7.1.8。两表按名字对齐,BSP 缺名(5.10→7.1.8 增删)的项:**照注册、索引取 7.1.8 id,记入报告**(DT 无法引用到它们,不影响真机消费)。
