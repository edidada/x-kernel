# RK3588 PCIe 移植画像(P0/P1)— pcie-dw-rockchip.c → x-kernel

## P0 采集 — 移植任务卡

| 字段 | 值 |
|------|----|
| Linux 源 | `drivers/pci/controller/dwc/pcie-dw-rockchip.c`(870 行,参考树 `linux` 已 vendor)+ DW 核心 `pcie-designware.c`/`pcie-designware-host.c`(寄存器与 host 约定)|
| compatible | `rockchip,rk3588-pcie` + `snps,dw-pcie`(板载 5 个 RC:pcie3x4@fe150000、pcie3x2@fe160000、pcie2x1l0@fe170000、pcie2x1l1@fe180000、pcie2x1l2@fe190000)|
| bus / 发布 | `drivers/bus/pci` 扩展:非 ECAM ConfigurationAccess(DBI+iATU),RC probe 后交给既有 PciBus 枚举 |
| x-kernel 落点 | `drivers/bus/pci/src/{dw_host.rs}`(RC 平台驱动+配置访问)或新 crate `drivers/bus/pci-dw`;枚举复用 bus/pci discover |
| DT(真机) | `orangepi-5-plus-m.dts`:reg 两窗口(`pcie-apb` 0xfe180000+0x10000、`pcie-dbi` 0x3040c00000+0x400000),6 clocks(aclk_mst/slv/dbi/pclk/aux/pipe),2 resets(pcie/periph),`rockchip,pipe-grf`,ranges(IO 0xf3000000+1M、non-prefetch 0xf3200000+14M、prefetch 64 位 0x9c0000000+1G),interrupt-map 4×INTx,msi-map→ITS(首版不用)|

### DT 绑定编号注意

- clocks/resets 索引是 BSP 头编号(CRU 索引,`bsp-opi5plus` 参考树已 vendor;`<0x02 0x151>` = CRU 输出 0x151)。
- `assigned-clocks` CRU 已在 CRU 移植中应用;RC 侧 clocks 由 `dev.res().clk("aclk_mst")` 等按名取(clock-names 已解析)。

## P1 分析 — 驱动画像

### 1. probe 流程(`rockchip_pcie_probe` → `rk3588_add_pcie_port`)

1. `devm_platform_ioremap_resource_byname("apb")` + `"dbi"` 两窗口。
2. `rockchip_pcie_resource_get`:clocks ×6、resets ×2、`rockchip,pipe-grf` syscon、max-link-speed/num-lanes。
3. `rockchip_pcie_phy_init` → PHY init/power_on(gap:phy 框架,req-phy in-progress)。
4. `reset_control_deassert` 顺序:`perst`→`core`→`pipe`→`aux`→`mgmt`…(rk3588 两线:pcie/periph;时序对照参考 `rk3588_pcie_deassert_perst`? 实际 7.1.8 按党 `rockchip_pcie_assert_perst` + 延时)。
5. `dw_pcie_host_init`:DBI 版本探测、`dw_pcie_iatu_detect`、初始化 `PCIE_PORT_LINK_CONTROL`/`LINK_WIDTH_SPEED_CONTROL`、outbound iATU 按 `ranges` 编程(MEM/IO/PREFETCH)、`dw_pcie_setup_rc`。
6. LTSSM 启动(`PCIE_CLIENT_GENERAL_CON` bit ltssm_enable)→ `dw_pcie_wait_for_link`。
7. INTx 域(legacy-interrupt-controller:APB `PCIE_CLIENT_INTERRUPT_STATUS` 4 bit,level→masked via `PCIE_CLIENT_INT_MASK`);MSI 走 ITS(gap:msi,req 已 rejected-for-v1 → PCI 设备用 INTx)。
8. `pci_scan_root_bus`(x-kernel:复用 bus/pci discover,ConfigurationAccess 换成 DW iATU 后端)。

### 2. 依赖子系统(scan 命中 → 决策)

| Linux API | 用途 | 决策(需求 issue) |
|-----------|------|------------------|
| `clk_prepare_enable` ×6 | 挂钟 | ✅ CRU 已落地(`dev.res().clk`) |
| `reset_control_deassert` ×2 | 控制器复位 | req-reset **in-progress**(CRU 软复位域为首个 provider) |
| `phy_init`/`phy_power_on` | PCIe PHY(combphy/pcie3phy) | req-phy **in-progress**(最小 provider 框架 + 2 后端) |
| `devm_gpiod_get`(reset-gpios) | PERST# | req-gpio **rejected**:假定固件已释放 PERST,probe 日志标注假设(power_reset_pin=works-around 分支) |
| `devm_regulator_get` | vcc3v3 供电轨 | req-regulator **rejected**:固定轨,假定固件已使能 |
| `msi-map`/ITS | MSI-X | v1 **不用**(INTx 足够枚举+基本设备);req-msi 保持 open |
| `udelay`(DBI/iATU/LTSSM 轮询) | 延时 | ✅ `khal::time::busy_wait`(E-05 同类,例外已覆盖 drv/clk;bus/pci 是宿主侧树,不受 L0-b 约束) |

### 3. 寄存器序列(编号参考清单 — 铁律 1,实现时逐条对照)

真相源:linux-7.1.8 `pcie-dw-rockchip.c` + `pcie-designware.c` + `pcie-designware-host.c`(全部 vendor)。

1. **APB 客户端模式**:`PCIE_CLIENT_GENERAL_CON`(0x0000)初值读改写;`PCIE_CLIENT_TYPE0_MODE`?(对照 `rockchip_pcie_host_init`:`PCIE_CLIENT_GENERAL_CON` 设 device type=RC(`PCIE_CLIENT_CONF_ENABLE`|`PCIE_CLIENT_LINK_TRAIN_ENABLE`|`PCIE_CLIENT_MODE_RC`))。
2. **iATU**:unroll 版(`PCIE_ATU_UNR_*` 经 `PCIE_ATU_VIEWPORT`/`iATU obs` 索引)—— 以 7.1.8 `dw_pcie_prog_outbound_atu_unroll` 为准;`PCIE_ATU_TYPE_MEM/IO/PREFETCH`/`CFG0/CFG1` 交替用于 type0/type1 配置访问。
3. **LTSSM**:`PCIE_CLIENT_GENERAL_CON` ltssm_enable bit;状态读 `rockchip_pcie_get_ltssm`(APB `PCIE_CLIENT_LTSSM_STATUS`)。
4. **INTx**:APB `PCIE_CLIENT_INTERRUPT_STATUS`(legacy 4 bit)、`PCIE_CLIENT_INT_MASK`;edge→level 语义对照 `rockchip_pcie_intx_handler`(ack 写 1 清)。
5. **DBI**:`dw_pcie_dbi_ro_wr_en`(PCIE_MISC_CONTROL_1_OFF)保护受写保护字段。

位段声明进 `data/bitfield-defs.json`(scope=port rk3588-pcie),由 `ldp check-layout --port rk3588-pcie` 守护。

### 4. 与 v2 bus/pci 的接缝

- v2 `drivers/bus/pci` 现有 `MmioCam`(ECAM,virtio/QEMU)。DW 路径提供第二个 `ConfigurationAccess` 实现:`DwConfigAccess { dbi: IoMap, apb: IoMap, cfg_windows: [iATU idx;2] }`,`PciRoot<DwConfigAccess>` 复用现有枚举。
- RC 平台驱动 probe 成功后通过扩展服务(或 driver-host ProviderClass)发布 `PciRoot`;组合根在 RC probe 完成后触发 PciBus 枚举(挂在其段号 `linux,pci-domain` 下)。
- `bus/**` 是宿主侧树(边界文档明确豁免 L0/L1),可直接依赖 khal/kclk。


---

## 实测进展（2026-10-30，诊断轮次 3）

**已证实工作**：
- fe190000（pcie2x1l2）probe 链路走通：6 clocks + 2 resets（req-reset）+
  combphy（req-phy，mode 绑定 PCIE）+ PD 电源域检查；
- **LTSSM L0 链路训练成功**：client LTSSM status = 0x00130011
  （bits 17:16 = 3 = link up；state = 0x11 = L0）——M.2 槽设备在位且
  训练完成，combphy/pipe/时钟全链路正确；
- client 寄存器窗口（fe190000）读写正常（GENERAL_CON 读回 0x4c = 
  mode RC | 状态位）；
- PD_PCIE repair-status 读回 BIT(22)=1（电源域已上电——U-Boot/硬件
  默认态）。

**当前卡点**：DBI 窗口（0xa41000000）首次读挂总线（CPU 停止、无 panic）。
同窗口的任何偏移访问均挂（0x0 读、0x8bc 写）。而 client 窗口、PD 状态
寄存器（0xfd8d8000+0x290）等 32-bit PA 访问全部正常。

**已排除**：时钟（6 个 enable 成功且父链递归开根门）、复位对、PHY、
关键根时钟（critical walk）、PD 电源、地址映射（linear map 正确打日志）、
MAIR 属性（nGnRnE 与 Linux 相同）、ranges/时钟 id 十六进制误读。

**下一轮诊断（二选一假设）**：
1. 36-bit PA DEVICE 访问路径首次被驱动——PTE/属性正确但 rk3588 总线
   对高 DBI 别名的访问需要额外使能（NIC 子系统门控/PHP GRF 位）；
   对照实验：读 fe170000 的 dbi 别名（0xa40800000?）与 fe180000 的
   （0xa40c00000），若全部挂 → 系统性；若部分响应 → 地址核对问题。
2. DBI 窗口需要 PERST# 先走一个完整脉冲（BSP 在 resource_get 时把
   PERST 拉低、phy init 后释放——我们从未动过 PERST，假设固件已释放；
   若固件实际把 PERST 拉低着，控制器状态机可能停在特殊态）——需要
   GPIO 框架或板级 EP 支持。

**注意**：链路 L0 意味着 PHY/pipe/时钟域全部工作，仅 DBI 寄存器
访问路径挂——不是电源域问题（repair status 已上电）。


### DBI 挂起诊断轮次 4（2026-10-30，MAIR + 双窗口 + 门控读回后）

MAIR Device 从 nGnRnE (0x00) 改为 nGnRE (0x04)（对齐 Linux ioremap）——DBI 仍挂。
CRU 门控读回：CLKGATE_CON(32/33/34/35/38) 全 0x00000000（所有 pcie 域门开）。
CLKSEL_CON(165) = 0x00000000（center mux 选 parent 0，正确）。
LTSSM = L0（链路已训练，槽上设备在位）。PD repair status = 已上电。

**结论**：时钟/复位/phy/电源域/映射/内存属性全部排除。
剩余假设需要硬件级对比：BSP 内核同板寄存器 dump、JTAG、或
向 Rockchip/BSP 供应商确认 pcie2x1l2 DBI 窗口（0xa41000000）的
访问前提（MMU_PCIE bypass？PHP NIC 门控？PERST 时序？）。

**建议路径**：切到 pcie3x4（fe150000，dbi @ 0x9c0000000，pcie30phy
聚合模式，PD 已证上电）作为下一个 probe 目标——不同控制器/不同
dbi 切片，可判别是 2x1l2 板级问题还是通用问题。

## 真机 bring-up 记录

踩坑与排查过程（含被证伪的假设、寄存器对照基准、环境坑、当前卡点）见
[`bring-up-log.md`](./bring-up-log.md)。在继续 port 之前先读它。
