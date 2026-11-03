# RK3588 PCIe / USB 移植 — 现状与交接（压缩前快照）

> 本文件顶部章节是**给下一次会话的自包含交接**。后面章节是历史踩坑记录
> （每条都标注了"假设 → 实测 → 教训"），继续排查前先读第 7 节。

## 0. 一句话现状

**PCIe 已经通了**：链路训练到 L0、DBI 可读写、iATU 配置成功、设备枚举结果与
同一块板上原生 Linux 的 `lspci` **逐条一致**（RC + RTL8125），两个真实设备已
注册进设备模型。**USB 尚未开始。**

## 1. 环境与操作手册（每次都要用，别重新摸索）

### 1.1 设备机与自动化

- 设备机：`ssh kylin@172.30.193.53`（BatchMode 免密可用）。
- **板级闭环**：`scripts/board-run.sh <tag>` = 构建 → 部署到 TFTP 根 →
  rkt 上下电 → 引导 → 抓控制台 → 取回日志。`SKIP_BUILD=1` 复用已构建镜像。
  日志在 `/tmp/board-<tag>.log`（本地）与设备机 `~/rkt/logs/<run>-<tag>/`。
- **TFTP 根**：`/media/kylin/USR-DATA/xh/work/rk3588-adapt/new-framework-test/x-kernel/`
  （`deploy-rk3588.sh` 推送到这里；内核镜像 `xkernel_aarch64-rk3588.bin`）。
- rkt 直接用法：`cd ~/rkt && ./rkt power on|off`、`./rkt tftp start`（**TFTP 随
  启动它的 SSH 会话退出**，报 `UDP 69 没有监听` 就先重启它）、
  `./rkt console --seconds N`。继电器帧 `AA 01 03 AE` 开 / `AA 01 02 AD` 关。
- 串口（换板/换线后会变，用前先枚举）：`/dev/ttyUSB0` 或 `ttyUSB2` 是控制台
  （1_500_000），继电器常见 `ttyUSB1`/`ttyUSB3`（115200）。判断"板子死了"之前
  先扫一遍所有 `ttyUSB*`。
- 设备机上的 `eaio_agent`（深信服终端代理）会抢占串口，导致"读就绪但无数据"，
  重试或让用户停掉它。

### 1.2 两条**必须**遵守的纪律

1. **unittest 必须用 QEMU 配置**：`cp platforms/kplat-aarch64/qemu_defconfig .config
   && make defconfig`，否则 `make unittest` 会静默挂死（Makefile 现在有
   `config-guard-qemu` 前置守卫会直接报错）。跑板子前则要 `rk3588_defconfig`。
2. **设备树是手工维护的产物，不要覆盖**。板上启动用的 DTS 在 TFTP 根同目录的
   `orangepi-5-plus-m.dts`（仓库里那份 `.dts` 是**反编译 dump，重编不等价**：
   phandle 会被重编号，`phys = <0x110 2>` 就指不到 combphy，PHY 起不来、链路
   永远 `link never came up`）。改设备树要改**设备机上的那份 .dts**，编前先备份
   `.dtb`（`cp -n x.dtb x.dtb.bak-$(date +%H%M)`）。

### 1.3 设备树里 PCIe 需要的 `config` 窗口（已改好）

5 个 PCIe 节点的 `reg`/`reg-names` 各加一段 `config`（1 MiB，基址 = 该节点
`ranges` 里 `0x800` 那条的 CPU 基址）：

| 节点 | config 基址 |
|---|---|
| `pcie@fe150000` | `0xf0000000` |
| `pcie@fe160000` | `0xf1000000` |
| `pcie@fe170000` | `0xf2000000` |
| `pcie@fe180000` | `0xf3000000` |
| `pcie@fe190000` | `0xf4000000` |

`reg` 顺序必须保持 `pcie-apb`(0) / `pcie-dbi`(1) / `config`(2)，与 `reg-names` 对应。

### 1.4 本地参考资料（都在磁盘上）

- **vendor 6.1 源码（板上跑的就是它）**：`/workspace/linux-jr`
  （sparse clone，分支 `noble`）。关键文件：
  `drivers/pci/controller/dwc/pcie-designware-host.c`（config 访问的权威实现）、
  `pcie-designware.h`（ATU 位段宏）、`arch/arm64/boot/dts/rockchip/rk3588s.dtsi`。
- **vendor 5.10 源码**：`/workspace/linux-bsp-rk3588`（分支 `orange-pi-6.1-rk35xx`
  也拉过）。注意 5.10 的 `rk3588s.dtsi` 里 pcie 节点只有两段 `reg`。
- **mainline**：`/workspace/linux-7.1.8`（pm-domains、phy、combphy 等）。
- **tgoskits**（另一份能工作的 rk3588 实现）：`/workspace/tgoskits`，
  `drivers/pci/rk3588-pci/`、`components/axklib/src/mmio.rs`。
- **ldp harness**：`xtask/ldp/` + `docs/ai/skills/linux-driver-port/`（api-mapping
  87 条全部锚定 v2 源码、参考树 sparse-vendor）。

## 2. PCIe 已验证的事实（不要重复验证）

| 项 | 结论 |
|---|---|
| DBI `0xa41000000` 可读写 | ✅（映射必须是 **Device**，见 3.1） |
| `setup_rc` / iATU / LTSSM | ✅ 链路 L0，`LTSSM=0x00130011`（与 Linux 相同） |
| 枚举 | ✅ `bus0 dev0` = RC `1d87:3588`；`bus1 dev0` = RTL8125 `10ec:8125` |
| 设备描述符进域 | ✅ 493 基线 + 2 = **494** |
| 复位 | 只 **deassert**，不要 assert 脉冲（脉冲会让 LTSSM 丢 bit20） |
| PD_PCIE | `PWR_CON(0x150)` bit7=1 才算上电；`repair(0x290)` bit22=1 表示**未**就绪 |
| `fe150000`/`fe160000` | APB 客户端块整块读 0 → 未上电，驱动**跳过**（不能碰 DBI，一碰就 stall 挂机） |

### 2.1 CFG（config space）寻址 —— 这一节是血泪

`config` 是 1 MiB 的 ECAM 形态孔径，但**必须**用 ATU region 0 覆盖它，且字段
来源是**分开**的（vendor 头文件自相矛盾，实测得出）：

```rust
// 每次访问重编 region 0（照抄 dw_pcie_other_conf_map_bus）
type_  = bdf.bus == 1 ? CFG0 : CFG1;
busdev = (bus << 24) | (function << 16);          // device 绝不能进 target！
prog_outbound_atu(0, type_, cfg_base, cfg_size /*整 1 MiB*/, busdev);
offset = (device << 15) | where;                   // device 走地址
```

- **device 放 target 的 bit[23:19] 无效**：那是
  `PCIE_ATU_FUNC_NUM(pf)`(bit20)/`FUNC_NUM_MATCH_EN`(bit19) 的位置，
  写进去只是打开 function 匹配。
- **function 放地址无效**：必须走 target `[18:16]`。
- **aperture 对 device≥1 有别名**（dev1..31 都回 dev0 的头）：用板级事实收口
  —— 每个控制器下游只有一个 endpoint 在 dev0/fn0（参考板 lspci 已证），
  驱动里把"下游 device≠0"判为 absent（`is_absent_downstream_device`）。
- region 大小必须是**整 1 MiB**；用 4 KiB 会把 device 偏移截断。

## 3. 三个反复踩的通用坑（对 USB 同样适用）

### 3.1 临时诊断不许改共享默认值

`memspace::iomap_device` 里曾有一行"`paddr >= 4 GiB` 就用 `UNCACHED`"的实验
代码没撤，导致 PCIe DBI（全内核唯一 >4 GiB 映射）一直是 **Normal-NC**，
后续三轮属性对比全部作废。**结论：DBI 必须 `Device`**（MAIR 用 nGnRnE 或
nGnRE 都能通，当前是 nGnRnE）。

### 3.2 两种总线失败模式（判故障用）

| 现象 | 含义 |
|---|---|
| `ESR=0x96000010`（ISS 0x10，DFSC=外部中止） | 总线返回 DECERR：**没东西解码该地址**，或访问属性被拒 |
| 静默 stall（无 panic、日志停住） | 事务被接收但永不完成：目标在，但没时钟/在复位 |

**存活探针**：某控制器 `pcie-apb` 客户端块若整块读 0，它就没上电，**绝不能
碰它的 DBI**（会 stall 挂机，`panic_on_oops=0` 也救不了）。

### 3.2b unittest 的 klogger 存量 flake（不是本分支引入）

`util/klogger/ut-tests/lib.rs` 的 `logbuf_*`、`set_log_level_takes_effect_immediately`、
`filter_spec_*` 以及 `procfs` 的 `printk_node_reads_and_writes_runtime_log_level`
都会改**全局**日志级别/缓冲区，并行跑必然互相踩：实测每次挂的具体条目都不一样
（`line 83/121/149/150/183` 都出现过）。父提交上同样挂，确认存量。看到
`make unittest` 只挂这几条就别查本分支。

### 3.3 用寄存器事实代替注释

- `PMU 0x290` 的 repair_status 注释写"1=上电"，但工作 Linux 上是 **0**。
- 状态寄存器**可能跨热重启保留**：早期把残留的 `LTSSM=0x130011` 当成"链路已
  训练"，白追几轮。判断链路要看**训练过程中的变化**（`0x2 → 0x30011`）。
- 映射建立后立刻 dump PTE（`page_table().query_entry(va)`）可一行否掉"地址
  算错/属性给错/被拆大页"。当前 `iomap` 对 >4 GiB 的映射会自动打印 PTE。

## 3.4 BAR：已分配、已迁移、**已证明可读**（闭环）

最终态（真机日志）：

```
pcie-dw: range config cpu 0xf4000000+0x100000 -> pci 0xf4000000
pcie-dw: range io     cpu 0xf4100000+0x100000 -> pci 0xf4100000
pcie-dw: range mem32  cpu 0xf4200000+0xe00000 -> pci 0xf4200000
pcie-dw: range mem64  cpu 0xa00000000+0x40000000 -> pci 0xa00000000 (prefetchable)
PCI BAR allocator from the DW host's DT ranges: cpu_base=0xf3200000 size=0xe00000
pci: 01:00.0 has BARs outside [0xf3200000, 0xf4000000); reassigning with decode off
pci: 01:00.0 BAR 2 [0xf3200000, 0xf3210000) (moved off firmware's address)
pci: 01:00.0 BAR 4 [0xf3210000, 0xf3214000) (moved off firmware's address)
pci: rtl8125 BAR0 at 0xf3200000 reads 0x126fd566 (MAC bytes), BAR is live
pci: registered 00:01.0 (10ec:8125 class 02:00) mmio [(0xf3200000,0x10000),(0xf3210000,0x4000)] io [(0xf3101000,0x100)]
```

这里有两个**互相独立**的坑，缺一不可：

### 3.4.1 窗口不存在：`pci-host-*-generic` 看不到 DesignWare 节点

`pci_bar_allocation_range()` 原来只问 `fw::pci_bar_range()`，而后者找的是
`compatible = "pci-host-ecam-generic"` / `"pci-host-cam-generic"`。RK3588 的
节点是 `rockchip,rk3588-pcie` + `snps,dw-pcie`，**永远找不到** → 返回 `None`
→ 分配器根本不存在 → 后面所有 BAR 逻辑都是空转（日志里连
"BAR allocator from ..." 都没有，这是最快的判断依据）。

修法：**控制器自己发布窗口**（它已经解析过 `ranges` 了），和它发布 CAM 访问
后端是同一个套路；`pci_bar_allocation_range()` 优先用这个窗口。

### 3.4.2 `ranges` 分类必须按 space 位，不能只看单个 bit

Rockchip BSP 的 `ranges` 形状（每段 7 cells）：

| 首 cell | 含义 | space 位 `[25:24]` |
|---|---|---|
| `0x00000800` | config 孔径 | 0 |
| `0x81000000` | IO | 1 |
| `0x82000000` | memory 32-bit | 2 |
| `0xc3000000` | memory 64-bit + prefetchable | 3 |

原来的判定是 `flags & 0x1`（IO）和 `flags & 0x2`（64 位），只对 mainline 的
`0x01/0x02/0x03` 成立。用单 bit 解码时 **config 段被当成 MEM 段**（于是 region 1
把 region 0 的地址区间又覆盖了一遍），**mem32 段被当成 mem64**。

**教训：`ranges` 的 `phys.hi` 只看 space 位 `[25:24]`（0/1/2/3），其余位是
类型信息，不同来源写法不同。**

### 3.4.3 "BAR 非 0" ≠ "BAR 已分配"

U-Boot 每次启动都会自己跑 PCI 枚举（串口里 `pcie_dw_rockchip ... Link Fail`
就是它打的），并按**它自己的地址空间**写 BAR。于是 BAR 寄存器非 0、读回也正确，
但地址落在 DRAM 区（实测 `0x40100000`），设备根本不可达。

修法：**"BAR 不在本 host 的窗口内" 也判为未分配** → 关解码 → 从窗口重分配 →
回读校验。

### 3.4.4 验证必须是一次真实的数据读取

写 BAR 再读回只能证明配置空间能写。唯一证据是**读到设备数据**：
RTL8125 的 BAR0 偏移 0 是 `IDR0..IDR3`（MAC 前 4 字节），读到
`0x126fd566`（非 0、非全 f）才算 BAR 活着。这条检查门控在单一已知设备、
一次一个 32 位读，等真驱动接管后删除。

## 4. RTL8125 驱动：probe 阶段已闭环

`drivers/drv/net`（新 crate）里的 `rtl8125` 驱动已通过 PCI 总线绑定成功：

```
rtl8125: RTL8125B xid 0x641 MAC [66, d5, 6f, 12, 66, 9e] BAR0 0xf3200000+0x10000
         cmd 0x00 intr 0x0000 rxcfg 0x0c02e60e
driver-core: 494 firmware descriptor(s), 35 active      # 之前 34
```

关键点（写进 crate 的 design.md/security.md 了）：

| 点 | 原因 |
|---|---|
| 型号来自 `TxConfig`，不是 PCI id | `10ec:8125` 覆盖 8125A/B/BP/CP/D，后续每条序列都按 MAC version 分支 |
| 先判 `~0` 再算 xid | BAR 未解码读回全 1，`(u32::MAX>>20)&0xfcf` 是个**看着合法**的 id |
| 站地址为 0/全 f 则 probe 失败 | 它是消费者唯一会用的值，垃圾地址比不绑定更危险 |
| **不发布到 `net` 类** | 能出现在系统里但服务不了队列的 NIC，会让网络栈把包投进没人取的队列 |
| probe 纯读、无写 | 重复 probe 安全；复位/PHY 等下一阶段 |

### 4.1 下一站（数据通路，按顺序）

1. **复位 + PHY**：`ChipCmd |= CmdReset` → 等清零 → 解锁 `Cfg9346` → 使能
   RX/TX。8125B 走 `RTL_GIGA_MAC_VER_63` 分支，时序与 8125A 不同。
2. **描述符环**：TX/RX 各 256，全 coherent DMA（`kdma`），**先轮询**。
3. **发布 `net` 类**：实现 `subsys_net::NetDevice` 并 `dev.publish(&NET, …)`。
4. **中断**：`kirq` + devres IRQ 守卫替换轮询。
5. **多 host**：`PUBLISHED_CAM` 仍是单槽，板上**第二块** 8125（另一控制器，
   `0004:41.0`）还不会进域。

## 4.1 数据通路：环和发布都通了，但 MAC 不取描述符（当前卡点）

### 已闭环的部分

```
rtl8125: RTL8125B phy 001c:c840 bmcr 0x1040 link up          # PHY 活着，链路 up（网线插着）
rtl8125: rings live, tx 256x2048B @ 0x203000, rx 256x2048B @ 0x204000
rtl8125: RTL8125B published to the net class as a NIC
knet:  use NIC 0: "rtl8125"                                  # knet 接手，建了 eth0
eth0             UP        66:d5:6f:12:66:9e <BROADCAST,MULTICAST,UP,LOWER_UP>
```

- `subsys_net::NetDevice` 全部实现（`send`/`recv`/`recycle_*`/`alloc_tx_buf`），
  双向都是**拷贝**模型：描述符所有权保持单写者，比零拷贝简单且不易错。
- 缓冲：ring 与 slot 都用 `dev.res().alloc_coherent()`（UNCACHED|SHARED），
  bus 地址 `0x203000/0x204000` 与 slot `0x3400000/0x3480000` 都是有效 DRAM。
- 用户态能看到网卡并 `ip link set eth0 up`，`LOWER_UP` 说明载波在。

### 卡点：描述符写进去了，MAC 一次都没读

```
pci: DeviceFunction{bus:1,device:0,function:0} command 0x0007 (bus master on)
rtl8125: tx[0] len 42 desc 0xb000002a -> 0xb000002a reclaimed false cmd 0x0c intr 0x0000 rx[0] 0x80000800
3 packets transmitted, 0 received, 100% packet loss
```

解读（每一条都是实测，不是推断）：

| 观测 | 含义 |
|---|---|
| `0xb000002a` 写后**原样不变** | MAC 没有取这个描述符（OWN 位没被清） |
| `intr 0x0000` | MAC **没有产生任何事件**：没有 TxOK，也没有 TxDescUnavail |
| `rx[0] 0x80000800` | RX 环也没被动过（活跃 LAN 上会有广播帧）；**收和发同时死** |
| `cmd 0x0c` | 两个引擎按寄存器看都是使能的 |
| PCI `command 0x0007` | Bus Master 已置（和 U-Boot 一致），DMA 权限没问题 |

**收与发同时、静默地全死**，指向"MAC 的 DMA/队列引擎没跑起来"，而不是
描述符格式或某个方向的配置错。

### 已被实测排除的假设（不要重复验证）

1. **doorbell 寄存器错**：8125 用 `TxPoll_8125 (0x90)` 的 16 位写 bit0，
   不是老的 `TxPoll(0x38)=NPQ`。已改对，现象不变。
2. **"new TX descriptor format" 没关**：OCP `0xeb58` bit0 读回 **0**（已是
   legacy 16 字节格式）。而且 MAC-OCP 隧道做过自检：写 `0xea84` 的 bit0/1
   读回 `0x0003`，所以"读回 0"是真值，不是隧道坏了。
3. **MAC OCP 隧道不可用**：同上，自检通过。
4. **`CPlusCmd` 的 `PCIMulRW`(bit3)**：写 0x28 读回 0x2020 —— 该位在本芯片
   只读 0，不是缺配置。
5. **PCIe 低功耗（ASPM / ClkReq / L2L3）**：`Config2/3/5` 的三位本来就都是 0。
6. **DMA 地址翻译**：`PHYS_BUS_OFFSET=0`（恒等），ring/slot 的 bus 地址都有效。
7. **iATU inbound 窗口未编程**：板上 DT（含 BSP `rk3588s.dtsi`）**没有
   `dma-ranges`**，Linux 在 host 模式下同样不编 inbound，所以不是这个。
8. **启动顺序**：已按 Linux 的顺序（先 `ChipCmd` 使能、后写
   `RxConfig/TxConfig`）跑过，现象不变。
9. **EPHY 初始化**：U-Boot 的 `eth_rtl8169` 在同一块板上 DHCP 成功，而它
   不做 EPHY 表，所以 EPHY 不是 DMA 的前提。
10. **`rtl_hw_start_8125_common` 的整块 OCP 魔法**：已逐条转写（含 0xa00 中断
    合并块清零、`0x382=0x221b`、`RSS/Q_NUM=0`、`MISC` 的 RXDV gate、以及
    `0xe00e` 等待），现象不变。

### 4.1.1 这一轮的新事实（RX 通、TX 半通）

**参考板是 vendor `r8125` 驱动**（不是 r8169），插上网线后是可用的工作实例。用它
做寄存器对拍 + 读 tgoskits 的 `drivers/net/realtek-rtl8125`（同 SoC 同芯片的 Rust
实现）后，确认并修复了两个真错误：

1. **8125 的中断寄存器偏移不是 816x 那套**：`IntrMask = 0x38`、`IntrStatus = 0x3c`
   （都是 32 位），不是 `0x3c/0x3e`。之前一直读 `0x3e`，所以**永远看到 0**，
   "MAC 毫无反应"这个结论是读错寄存器读出来的。
   修正后立刻看到真实状态：

   ```
   tx[0] ... intr 0x0081        # RX_OK(bit0) + TX_DESC_UNAVAILABLE(bit7)
   tx ring probe: 256 of 256 descriptors returned, intr 0x0084
   ```

   - `RX_OK` 持续置位 → **收包路径是通的**（MAC 在收）。
   - 把整条 TX 环 256 个描述符全部填成合法帧再踢一次门铃，**256 个全被消费**、
     `TX_OK` 置位 → **发送也能工作**，环、DMA、门铃、MAC 都不是坏的。
2. **`rtl_hw_init_8125` 缺失**：mainline 在 `rtl_hw_reset` **之前**要清
   `MCU(0xd3)` 的 `NOW_IS_OOB`、写 OCP `0xe8de`、并等 `MCU & LINK_LIST_RDY`。
   已按参考实现补上（本板一直 ready，但不是可省的步骤）。

顺带确认无效、别再做的：`CPlusCmd` 的 bit3/bit4（PCIMulRW / PCI_DAC）在本芯片
**只读 0**，写了读回不变；`McAfee`… 抱歉，`Config1/2/5` 的"关低功耗"三位在参考板
上本来就是开着的（我们改它们反而偏离工作实例）；把环从 `0x203000` 搬到
`0xe8000000` 对现象无影响（所以低地址 DMA 可达，之前那次"参考板改基址就 ping
不通"的实验有 confound：环内容和地址一起变了，不能作为证据）。

### 4.1.2 当前唯一卡点：单帧提交不被消费

```
tx[0] len 42 desc 0xb000002a -> 0xb000002a reclaimed false   # OWN 位没被清
txpoll 0x0000                                                # 门铃被吃了
intr 0x0085                                                  # RX_OK + TX_OK + DESC_UNAVAIL
txdesc 0xb000002a 0x0000000e 0xffff7ddf 0xffffff7f .. 0        # 各描述符 opts1
```

整环 armed 时全部被消费，**单个描述符 armed 时不动**，而且相邻槽位出现
`0xffff7ddf` / `0xffffff7f` 这类我们还没解码的写回值。下一步应当：

1. 用**同一份 tgoskits 驱动**在同一块板上跑一遍（他们有完整的 TX/RX 队列实现），
   如果他们也只通 RX，那就是硬件/RC 侧问题；如果 TX 也通，逐步回退我们对齐差异。
2. 解码 `0xffff7ddf` 这类 TX 写回值（r8169 的 `TxDesc` 状态位表 + `rtl8169_tx_vlan_tag`
   之外还有 `TxStatus` 位段），确认 MAC 是把这些槽位当作"已发送"还是"错误"。
3. 检查 `RingEnd` 与**描述符数量**是否必须成 2 的幂且起点对齐（我们的环 4 KiB，
   `alloc_above` 用 16 MiB 块，首地址 16 MiB 对齐）。

### 下一步（唯一还没做的决定性实验）

**和 BSP Linux 在寄存器级对拍。** 板子 SD 里有可启动的 BSP Linux
（`Image` + `dtb-6.1.99-rockchip-rk3588/`），手工 `booti` 的完整配方已写进
`docs/ai/skills/board-automation/SKILL.md` §5.7。登进去后：

```python
# /dev/mem 读 BAR0，和驱动里同样的偏移逐一对比
# 重点：0x20/0x24(ring base)、0x3c(IntrMask)、0x37(ChipCmd)、
#       0x40/0x44(Tx/RxConfig)、0x34、0x7a、0x90、0xe4/0xe8
```

差异就是缺的那一步。**不要再用"猜一个寄存器试一轮"的方式推进** —— 本轮
已经用掉 10 个假设，每一个都是"看起来很像"的。

## 4.2 U-Boot 每次启动都会枚举 PCI（影响 BAR 语义）

串口里 `pcie_dw_rockchip pcie@fe150000: PCIe-4 Link Fail` 是 **U-Boot** 打的
（在我们内核启动之前）。它按自己的地址空间写 BAR，所以内核看到的 BAR 可能
非 0 却不可用。`configure_device` 已把"落在本 host 窗口外"判为未分配。

## 5. USB 相关（尚未动工，只有前期结论）

- `usb2phy` provider 已在真机 probe 通过（grf 窗口 `0xfd5d0000/8000/c000`）。
- EHCI/OHCI **不依赖**高位 DBI 窗口，不受本轮 PCIe 问题影响。
- 待做：`subsystems/usb` 类面（hub/地址/描述符）+ EHCI/OHCI HCD（全 coherent DMA）。

---

# 历史记录（踩坑过程）

## 1. 当前状态

| 里程碑 | 状态 | 证据 |
|---|---|---|
| CRU 时钟树（9 PLL + 648 分支） | ✅ | 真机 SD 卡 ext4 根挂载、启动到用户态 |
| 复位 provider（639 行表） | ✅ | `SOFTRST_CON33/34` 读回：本控制器两条线已释放 |
| PHY provider（usb2phy/combphy/pcie3phy） | ✅ probe | combphy id 0 `hw_init` 完成，refclk 100 MHz |
| RC 时钟 6 路使能 | ✅ | `CLKGATE_CON(32..38)` 读回全 0（未门控） |
| PD_PCIE 电源域 | ✅（读数存在） | `REPAIR_STATUS(0x290)` bit22 = 1 |
| client APB 窗口 `fe190000` | ✅ 可读写 | LTSSM/HOT_RESET 等寄存器均可读 |
| **链路训练（LTSSM=L0）** | ✅ **已达成** | `ltssm 0x00000002`（Detect）→ `0x00030011`（LINK UP + L0），15 ms |
| **DBI 窗口 `0xa41000000`** | ❌ **stall** | 读永远不返回，无 panic、无 abort |
| 设备枚举 / BAR / INTx | ⬜ | 依赖上两项 |

**已走通的部分**：PHY → 时钟 → 复位脉冲 → PD_PCIE → 客户端 RC 模式 →
LTSSM 从 `0x2`（Detect.Quiet）走到 `0x00030011`（bits17:16=3 = LINK UP，
低 6 位 `0x11` = L0），耗时约 15 ms。**链路是真实训练成功的**。

**尚未走通**：`0xa41000000`（pcie-dbi 窗口）的 CPU 读返回同步外部中止
（`ESR=0x96000010`，DFSC `0x10` = DECERR），与链路状态无关——**整条 0xa
高位 PCIe 段都没有被解码**：同板上从未初始化过的兄弟控制器
（`a40c00000`，pcie2x1l1）行为完全一样。

DT 侧已逐项核对通过（从参考板 `/proc/device-tree/pcie@fe190000/` 直接读）：

```
reg       = 0xfe190000+0x10000 / 0xa41000000+0x400000 / 0xf4000000+0x100000
reg-names = pcie-apb pcie-dbi config          ← 三段（我们仓库里的 BSP dts 只有两段）
clocks    = <2 0x152> <2 0x157> <2 0x14d> <2 0x15d> <2 0x162> <2 0x182>
clock-names = aclk_mst aclk_slv aclk_dbi pclk aux pipe   ← 与 BSP dts 逐字相同
phys      = <0x110 2>                          ← combphy0 @fee00000，PHY_TYPE_PCIE
```

即：地址、时钟编号、PHY 引用三者都与板上实际 DTB 一致，不是 DT 解析问题。

!!! 顺便纠正一个早期结论：**"pipe 时钟死了"是错的**——链路既然能从
Detect 走到 L0，pipe 就必须在跑。（那是把"DBI 不可达"错误归因到时钟链上：
后来证明 DBI 的 DECERR 与时钟、复位、电源域、链路状态**全都无关**。）

---

## 2. 被证伪的假设（重要：不要重复走）

按"曾经多有说服力 → 实测怎么否掉"排序。每条都值得记住，因为**下一个人会
再次想到它们**。

### 2.1 PTE 的 NS 位（Non-secure PA space）

- **推断**：`Arm64Attr` 只在 `SHARED` flag 下置 NS=1，设备映射 NS=0 =
  Secure PA space，non-secure EL1 访问高位 DBI 被 TZPC 阻断 → 挂。
- **动作**：改成无条件 `a |= Self::NS`。
- **实测**：DBI 仍然 stall，行为毫无变化。改动保留（无条件 NS=1 本身是对的，
  与 Linux 一致，但没有解决本问题）。
- **教训**：32 位外设全部正常时，"这是个地址空间/权限问题"的叙事特别顺，
  但它同时也能解释任何东西 —— 这种"什么都能解释"的假设优先级要放低。

### 2.2 Device 内存属性（nGnRnE / nGnRE / Normal-NC）

- **推断 A**：rk3588 的高位 DBI 段不接受 Device-nGnRnE → 改 nGnRE。
- **推断 B**：tgoskits（同样 rk3588、能工作）用的是线性映射（Normal 语义）
  → 全局把 Device MAIR 改成 Normal-NC(0x44)。
- **实测**：推断 B 让**内核在 "Starting kernel ..." 之后彻底静默**（UART 等
  早期设备映射语义被一起改掉），已 revert；随后改成"只对 >4 GiB 的 iomap 用
  UNCACHED"，DBI **仍然 stall**。
- **教训**：属性不是根因。全局改 MAIR 这种"一把梭"实验代价极高且信息量低，
  先做单变量、单窗口的最小改动才划算。

### 2.3 CLKREQ# 拉低导致控制器进入低功耗态

- **推断**：`PCIE_CLIENT_POWER = 0x30011000`（拉低 CLKREQ）写在首次 DBI 访问
  之前，把控制器"停"了；BSP 是先 DBI 再写它。
- **动作**：把 DBI 读移到 client APB 写之前。
- **实测**：仍然 stall。且 tgoskits 在首次 DBI 访问**之前**就写 CLIENT_POWER +
  GENERAL_CTRL，一样能工作 → 顺序本来就不是问题。
- **教训**：把"顺序"当成根因之前，先找一个同样顺序但能工作的实现来对照。

### 2.4 PMU 总线 idle 请求（BUS_IDLE_REQ）

- **推断**：`PMU_BUS_IDLE_REQ` 把 PCIe 的 NOC 段 hold 在 idle，访问永久 stall。
- **实测**：我方 `0xfd8d810c = 0`（没有任何 idle 请求），Linux `= 0x17ff`（别的
  域 idle）。参考内核 `rk3588_pm_domains` 表里 `RK3588_PD_PCIE` 的 `req_mask == 0`
  → **PCIe 域根本不参与 idle 握手**。
- **教训**：`pm-domains.c` 的 `DOMAIN_RK3588(...)` 参数顺序必须展开核对
  （`p_offset, pwr, status, m_offset, m_status, r_status, r_offset, req, idle, ...`），
  否则会把"另一个域的位"当成目标域的。

### 2.5 复位只 deassert（不 assert）

- **推断**：BSP 5.10 只 deassert，我们照做。
- **动作**：改成 assert → 1 µs → deassert 的真脉冲（对齐 mainline 与 tgoskits）。
- **实测**：仍然 stall。脉冲是对的（已保留），但不是根因。
- **教训**：复位脉冲属于"无论如何都该做对"的正确性修复，不要期待它治病。

### 2.6 `reg` 解析步长 / PTE 地址截断 / VA 冲突

- **推断**：`#address-cells=3` 的节点用错步长 → DBI 基址算成垃圾；或 PTE 只
  保留 32 位把 `0xa41000000` 截断成 `0x41000000`；或 iomap VA 与线性 RAM 冲突。
- **实测**：日志直接打印解析结果 —— `dbi 0xa41000000+0x400000` **完全正确**；
  `PADDR_MASK = 0x0000_ffff_ffff_f000`（bit12–47）不截断；`TCR_EL1::IPS::Bits_48`。
- **教训**：这些都用**一行日志**就能否掉。先打印事实，再谈假设。

### 2.7 把热重启残留值当成"链路已训练"

- **现象**：早期版本读到 `APB 0x300 = 0x00130011`（bit17:16=3 → link up），于是
  长期以为"链路 L0 已通，只剩 DBI 不响应"，把排查引向纯 CPU 侧。
- **真相**：client APB 块在 always-on 域，**寄存器值跨热重启保留**。
  `0x130011` 是历史残留（和 Linux dmesg 里那句 "Link up, LTSSM is 0x130011"
  一字不差，正是这个巧合骗过了我）。实测训练中的真实值是 `0x3`。
- **教训**：**任何"状态寄存器"读数，先问它是否跨复位保留**。判断链路是否真的
  训练，要看"训练过程中它变化"（`0x3 → 0x130011`），而不是看一个瞬时快照。

---

## 3. 真正有效的排查手法

### 3.1 用两种失败模式区分"没路由"和"没时钟"

同一地址在不同初始化阶段访问，行为不同，这个差异本身就是诊断信号：

| 现象 | ESR / 表现 | 含义 |
|---|---|---|
| Synchronous external abort | `ESR=0x96000010`，DFSC=`0x10` | 总线返回 DECERR：**没有东西在解码这个地址**（块未供电/未开时钟） |
| 静默 stall | 不 panic、不 abort、日志停住 | 事务被接收但**永不完成**：目标可达，但它的 core 时钟没跑 |

实测序列（本控制器）：

```
初始化之前读 0xa41000000 → external abort（DECERR，和未初始化的兄弟控制器一样）
时钟+复位+PHY 之后    → stall（路由通了，但从设备不响应）
未初始化的兄弟 1l1    → external abort
```

"从 abort 变成 stall" 精确地指出：时钟/复位确实生效了，缺的是**让 core 真正
跑起来的那个时钟**。

### 3.2 与工作内核逐寄存器对照（本问题的终结手段）

板子上跑一份 BSP Linux，拿到 root（Orange Pi 5 Plus 的 `orangepi` 账户 sudo
密码就是 `orangepi`），用 `/dev/mem` 读，和我们内核里同样的偏移逐字对比。

参考值（stock Linux 6.1.99，PCIe 工作正常，详见
`docs/ai/skills/board-automation/SKILL.md`）：

```
DBI   0xa41000000 = 0x35881d87          ← 我们这里 stall
APB   0x00 = 0x4c  0x04 = 0x04  0x10 = 0x03  0x14 = 0x122
      0x18 = 0x7737  0x20 = 0x177f  0x2c = 0x2008  0x30 = 0x04401812
      0x180 = 0x12   0x300 = 0x00130014     ← 我们这里是 0
CRU   0x440 = 0x288（aclk_php_root = gpll/3 = 396 MHz，我们一致 ✓）
      0x884 = 0x739c（aclk_pcie_1l2_dbi 门 = bit1 = 0，已开；我们一致 ✓）
PMU   0x290 = 0xfe9f0001（bit22 = 0）        我们 0xfff70001（bit22 = 1）
combphy0 0x2c = 0x43 / 0x30 = 0x88  我们 0x02 / 0x08
pipe-phy-grf 0x34 = 0x80                      我们 0xc0
时钟速率 aclk_1l2_dbi/slv 396 MHz、mstr 500 MHz、pclk 150 MHz、aux 24 MHz
```

**注意 PMU `0x290` 的坑**：mainline 注释写 "1'b1: power on"，但工作正常时该位是
**0**。`rockchip_do_pmu_set_power_domain()` 里 power-on 写的是
`pwr_w_mask`（值位为 0），即该位=0 才是上电。**注释和寄存器的语义不要照抄，
以工作实例的读数为准。**

### 3.3 用 `clk_summary` 拿"应有的时钟速率"

`/sys/kernel/debug/clk/clk_summary`（root）直接给出工作系统里每一路时钟的实际
频率，是验证我们 CRU 分频/选择是否正确的唯一硬标准。本处理器实测：
`aclk_pcie_1l2_dbi = 396 MHz` 正好等于 `GPLL(1188)/3`，与我们
`CLKSEL_CON(0x440)` 的 div 字段=2 吻合 → 时钟树这一层是**对的**，直接把
排查范围压缩到 PHY 侧。

---

## 4. 环境坑（每轮都要面对的）

1. **换板后串口会重编号**。real board 从 `ttyUSB0/1` 变成 `ttyUSB2`（控制台
   1_500_000）/ `ttyUSB3`（继电器 115200，帧 `AA 01 03 AE` 开 / `AA 01 02 AD` 关）。
   判断"板子死了"之前先扫一遍所有 `ttyUSB*`。
2. **TFTP 服务随启动它的 SSH 会话退出**。报 `UDP 69 没有监听` 时先
   `rkt tftp start`，别怀疑镜像。
3. **`board-run.sh` 结束时会把板子断电**。要和板上的原生 Linux 对话，先
   `rkt power on` 并等 40–50 s 启动。
4. **unittest 必须用 QEMU 配置**。`.config` 停在 rk3588 时 `make unittest` 会静默
   挂死；Makefile 的 `config-guard-qemu` 现在会显式报错（曾经被它救过一次）。
5. **`IoMap::read32` 会校验 4 字节对齐**。诊断代码里写
   `for off in 0..0x40 { apb.read32(off) }` 会立刻 panic
   （`MMIO access misaligned`）——看起来像硬件挂了，其实是自己的 bug。
   正确写法 `(0..0x40).step_by(4)`。

---

## 5. 当前卡点：PHP APB 门被"使能后立刻撤销"

**实测证据（`CLKGATE_CON(32) @ 0x880`）**：

```
kclk-gate: 0x880 bit 0 enabled=true  write=0x00010000   ← 使能（正确）
kclk-gate: 0x880 bit 0 enabled=false write=0x00010001   ← 1.4 ms 后又被关掉
```

`0x880` bit0 = `pclk_php_root`（`SET_TO_DISABLE|HIWORD_MASK`），是整个 PHP 域的
APB 根时钟。它被关掉 → combphy 寄存器**全部读回 0** → PHY 无法工作。
`Gate`/`Mux`/`Divider` 三条写路径的 hiword 掩码本身都正确（已逐条核对，
`0x440` 邻居字段全程未被改动），所以**不是"误清邻居"，而是有人紧接着把门关回去**。

`ClkHandle::prepare_enable()` 的错误分支会调用 `unprepare_locked()`（即 disable），
但复现时它返回 Ok；`combphy` 里对三路时钟逐个 `is_enabled()` 的结果是
`clock 0: false -> false`、`clock 1: false -> true`、`clock 2: false -> true`
（第二次 `enable()` 才真正留下使能状态），说明**第一次的使能在返回前被撤销了**。

补充实测（`enable failed` 与 `hw_exit` 两条日志都加了之后）：**回滚分支从未
触发，`hw_exit` 也从未执行**，但计数确实掉到 0 —— 所以撤销来自
`prepare_locked()` 的父链传播路径，而不是错误回滚。

**这条级联目前不影响结果**：循环结束时 `880/884/888/89c` 全为 0（所有门开），
PHY 寄存器读得真值，链路也能训练到 L0。临时对策（逐路 `is_enabled()` 校验 +
必要时再使能一次）保留即可，优先级降到最低。

### 级联链（`core/kclk` 里加 `hw-disable "<name>"` 日志后实测）

```
combphy: cru before-clocks ...            ← 开始使能三路时钟
kclk: hw-disable "clk_ref_pipe_phy0" (count 0)      kclk: hw-disable "clk_ref_pipe_phy0_pll_src"         |  refclk 及其父链被关
kclk: hw-disable "ppll"                              |
kclk: hw-disable "pll_ppll"                         /
combphy: clock 0 ... enabled false -> false        ← refclk 是纯 mux，is_enabled 恒 false
kclk: hw-disable "pclk_pcie_combo_pipe_phy0"       ← apbclk 被关
combphy: clock 1 ... enabled false -> true
kclk: hw-disable "pclk_php_root"                   ← PHP APB 根门被关
combphy: clock 2 ... enabled false -> true
```

规律是**每一路时钟 `prepare_enable()` 之后紧跟着它自己（及父链）被 `disable`**，
且 `prepare_enable()` 仍返回 Ok。`ClkHwOps` 的默认 `enable/enable/prepare` 全是
`Ok(())`，所以不是"对 mux 调 enable 报错触发回滚"这一条路；
`enable_locked()` 里唯一的 `disable_locked` 调用在错误回滚分支（会返回 Err）。
**这是下一轮要钉死的地方**：在 `kphy` 的 `init_powered()`/`PhyInstance` guard 与
`ClkHandle` 的 drop/refcount 语义之间，存在一条"使能后立刻释放"的路径。

注意：`ppll` / `pll_ppll` 正是 combphy pipe 输出的源，被关掉就意味着
**pipe 时钟没有源** —— 这与"LTSSM 恒为 0"完全自洽，所以修掉这条级联很可能
直接让链路训练起来。

### 已修：PD_PCIE 电源域（结论：不是最后一环，但状态已与参考一致）

原先的代码有三处错：偏移写成 `0x14c`（应为 `0x14c + 域偏移 0x4 = 0x150`）、
把 `repair_status` bit22=1 当成"已上电"直接返回、以及极性判断来自参考注释而非
实测。按工作内核的读数改正后：

```
修改前: PWR_CON(0x150)=0x00000011 (bit7=0)  repair(0x290)=0xfff70001 (bit22=1)
修改后: PWR_CON(0x150)=0x00000091 (bit7=1)  repair(0x290)=0xffb70001 (bit22=0)
参考  : PWR_CON(0x150)=0x000004c1 (bit7=1)  repair(0x290)=0xfe9f0001 (bit22=0)
```

域确实上电了，但 **`APB 0x300` 仍为 0、DBI 仍不可达**，所以 PD 不是最后一环
（记下来，避免以后再花一轮验证这件事）。

### 失败模式的变化（重要）

| 阶段 | DBI 访问 `0xa41000000` 的表现 |
|---|---|
| 任何初始化之前 | 同步外部中止（DECERR） |
| 只使能 refclk | **静默 stall**（接了但不响应） |
| 使能 refclk+apbclk+phpclk（当前） | **同步外部中止（DECERR）** `ESR=0x96000010 ISS=0x10` |

也就是说：现在的状态回到"这个地址不被解码"，即 PCIe 从设备口完全不在线；
`general-con = 0x44`，而工作 Linux 是 `0x4c`，差的 bit3 是 `APP_DLY2_DONE`
（只有 APP 侧 LTSSM 序列跑完才会置位的**状态**位）——两处证据都指向同一个结论：
**DWC core 的 APP/LTSSM 侧没有真正启动，pipe 时钟仍然不成立。**

### 已排除：GRF 侧完全没有缺口（第 N 轮，负面结果）

把 `PHP_GRF` 全 `0x000–0x11c` 与我们和工作 Linux 逐字对比：

```
  off      ours      linux
 0x40  00110010  00110010
 0x64  00008010  00008010
 0xe0  00001410  00001410
 0xe4  00002000  00002000
 0xe8  00001100  00001100      ← 之前怀疑的 lane/NOC 区
 0xf8  000007e8  000007e8
0x104  00000300  00000300
```

**72 个字全部逐位相同，没有一处差异。** 所以"PCIe lane/NOC 选择位缺失"这条
假设也被否掉了——GRF 不需要任何额外写入。

`pcie-apb` 客户端块（`0x00–0x3c`）的差异同样可以解释：我们的 dump 发生在
`configure_rc_mode()` **之前**，所以 `0x00/0x180/0x300` 全为 0，而 Linux 那些
值是它自己配好之后读的（`0x4c / 0x12 / 0x130014`）。不构成证据。

### 已排除：映射层没问题（同一 iomap 路径读 >4 GiB DRAM 成功）

```
pcie-dw: hi-ram 0x100000000 = 0x00000001    ← 同一 iomap 路径，>4 GiB，读成功
pcie-dw: reading own dbi 0xa41000000        ← 紧接着读 DBI
panicked: DataAbortCurrentEL (external abort)
```

DRAM（`0x1_0000_0000`）用**完全相同**的 `iomap_region` 路径能读，DBI 不能 ⇒
不是我们内核的 >4 GiB 映射/PTE 问题。同时 4 KiB 小映射
（`probe_dbi_page`）也没有读成功，排除"4 MiB 大映射被拆页搞坏"。

另：把 `GENERAL_CON` 精确写成 Linux 的 `0x4c`（含此前未知的 bit3）后
`ltssm 0x00030011` 仍是 L0，DBI 依旧中止 —— bit3 也不是那个开关。

### 失败模式分两类（这一轮的意外收获）

| 对象 | CPU 读 DBI 的表现 |
|---|---|
| 我们控制器（已初始化、链路 L0） | **同步外部中止 (DECERR)** = 地址没被路由 |
| 未初始化的控制器 `0xa40400000`（pcie3x2） | **CPU 直接挂死**（控制台只剩回车）|

后者是在**原生 Linux 板上**测出来的：`dd if=/dev/mem` 读一个从未被驱动碰过的
控制器 DBI，整机卡住（`panic_on_oops=0` 也没用，因为不是 oops 而是总线事务
永不完结）。结论：**DBI 窗口对未初始化控制器完全不可用**，Windows 的可用性
依赖控制器自己初始化过；两种失败模式（DECERR / stall）分别对应"没路由"和
"路由了但目标不响应"。

> 代价说明：这次读取把参考板挂住了，且继电器只控我们这块板，**需要人工断电
> 重启那台原生 Linux 板**。以后在参考板上做同类探测要先确认可断电。

### BSP 源码到手（`orange-pi-6.1-rk35xx`，板上跑的就是这版）

本地已稀疏克隆，**后续不用再猜，直接查**：

```
/workspace/linux-bsp-rk3588                    # --sparse, branch orange-pi-6.1-rk35xx
  drivers/pci/controller/dwc/pcie-dw-rockchip.c   # 2117 行（mainline 只有 870）
  drivers/phy/rockchip/                          # combphy
  drivers/soc/rockchip/                          # pm_domains / grf
  arch/arm64/boot/dts/rockchip/                  # 172 个 dtsi，含 rk3588s.dtsi
```

**6.1 BSP 的真实 probe 顺序**（`rk_pcie_really_probe`）：

```
rk_pcie_resource_get            // pcie-dbi / pcie-apb 两次 devm_ioremap_resource
rk_pcie_hardware_io_config:     // 4. hardware io settings
    regulator → clk_bulk_prepare_enable(6 路) → phy_init → phy_power_on
    → reset_control_deassert → **phy_calibrate()**      ← 见下
pm_runtime_enable + pm_runtime_get_sync
rk_pcie_host_config             // 5. 这里才有第一次 DBI 访问（dbi_ro_wr_en）
rk_pcie_init_irq_and_wq         // 6. 软件侧
rk_add_pcie_port                // dw_pcie_host_init
```

核对结论：

- **没有 rk3588 专属的 PMU/syscon 写**。5.10 里那三条 `pmu_grf 0x100=0x01000100` /
  `0x0=0x0c000000` / `usb_pcie_grf` 都在 `rk1808_pcie_fixup()` 里，被
  `is_rk1808` 挡住，与 rk3588 无关。
- `rk_pcie_set_mode()` 与我们一致（`PCIE_CLIENT_POWER=0x30011000`、
  `GENERAL_CON=0xf00040`）。
- `PCIE_CLIENT_LTSSM_STATUS` 的位定义补全：**bit16 = SMLH_LINKUP，
  bit17 = RDLH_LINKUP**。我们的 `0x00030011` 两位都是 1 → 链路确实 up；
  Linux 的 `0x001300xx` 多一个 bit20（未定义，疑为速率/带宽域）。
- 官方 5.10 的 `rk3588s.dtsi` 里 pcie 节点是**两段** `reg`（`pcie-apb` +
  `pcie-dbi`）；而板上实际 DTB 是三段（多 `config`）→ 板上跑的是更新版本，
  6.1 分支才是权威。

**新发现的缺口：`phy_calibrate()`**。BSP 在 `phy_init` + `phy_power_on` +
复位释放之后调 `phy_calibrate`，失败就报 `"phy lock failed"` 并回滚。我们的
combphy 没有实现它，而且**实测 combphy 寄存器 `0x2c`(0x02 vs Linux 0x43)、
`0x30`(0x08 vs 0x88)、`0x28/0x34` 也不一致**——这三个字很可能就是 calibration
写的。链路能训练说明 LTSSM 不依赖它，但它是否影响 DBI 从设备可就不好说了，
**这是当前最具体的可验证缺口**。

### 权威源码已确认（`Joshua-Riek/linux-rockchip`，noble = 6.1.75）

板的 DTB 是三段 `reg`，只有这棵树对得上，之前那份 orange-pi/5.10 是两段。
本地：`/workspace/linux-jr`（`--sparse`，分支 `noble`）。

**逐项核对结果：源码层面我们与 Linux 已经等价。**

| 项 | 权威源码 | 我们 |
|---|---|---|
| `reg` / `reg-names` | `pcie-apb` + `pcie-dbi`(0xa41000000) + `config`(0xf4000000) | 一致（解析出 `dbi 0xa41000000+0x400000`）|
| `resets` | `SRST_PCIE4_POWER_UP`, `SRST_P_PCIE4` | 一致 |
| `ranges` | IO 0xf4100000 / MEM 0xf4200000 / prefetch 0xa00000000 | 一致 |
| `phys` | `<&combphy0_ps PHY_TYPE_PCIE>` | 一致 |
| `power-domains` / `assigned-clocks` | **均无** | 不涉及 |
| PHY 步骤 | `phy_set_mode_ext` → `phy_init` → `phy_power_on`（**这棵树没有 `phy_calibrate`**）| 一致 |
| 复位 | `reset_control_deassert`（此前 phy_init）| 一致（我们多了 assert 脉冲，非关键）|
| 时钟 | `clk_bulk_prepare_enable` × 6 | 一致 |
| 首次 DBI 访问 | `dw_pcie_dbi_ro_wr_en()` | 同位置 |

> 注：`phy_calibrate()` 只存在于 `orange-pi-6.1-rk35xx` 分支，Joshua-Riek 6.1.75
> 没有 → 上一轮记的"calibration 缺口"对本板**不成立**，已作废。

### 剩下的唯一解释方向：内核之外

源码等价、寄存器等价（CRU/PHP_GRF/PMU/复位字/`GENERAL_CON` 全部实测一致）、
链路 L0，而 DBI 仍 DECERR。剩下的差异只能在**内核之前或内核之外**：

1. **启动路径不同**。参考板从 SD/eMMC 启动，我们的板子是 U-Boot `tftpboot`
   网络启动。如果 SD 那条 boot 流程里 U-Boot 对 PCIe 做过初始化（例如为了
   NVMe/网络启动而打开了 PCIe 的 NOC/DBI 通路），tftpboot 路径就不会做。
   **验证方法：让我们的板子也走 SD 启动跑同一个 tftpboot 内核**，或反过来在
   参考板上 tftpboot —— 这一步能直接把"bootloader 差异"证伪或坐实。
2. **安全寄存器（SMC）**。rk3588 有 `rockchip_sip`（SMC）接口，部分 NOC/DBI
   配置在安全侧，只能由 ATF 打开。Linux 自身在该分支里没有 PCIe 的 SMC 调用，
   但 bootloader 可能有。
3. 参考板是可断电的（这次事故后已确认 `panic_on_oops=0` 也拦不住总线 stall），
   所以任何"再读一个可疑地址"的实验都要先确认能断电。

### 结论翻转：我们这块板的高位 DBI 段**没有被解码**

U-Boot 侧实测（打断 autoboot，`U-Boot 2025.04`，mainline `pcie_dw_rockchip`）：

```
=> pci enum
pcie_dw_rockchip pcie@fe150000: PCIe-4 Link Fail
pcie_dw_rockchip pcie@fe170000: PCIe-5 Link Fail
=> pci
00.00.00 0x1d87 0x3588  Bridge      ← 枚举成功：2 个 RC + 2 个 RTL8125
01.00.00 0x10ec 0x8125  Ethernet
02.00.00 0x1d87 0x3588  Bridge
03.00.00 0x10ec 0x8125  Ethernet

=> md 0xfe190000 4
fe190000: 0000004c 00000000 00000000 00000000   ← APB 窗口：正确（GENERAL_CON=0x4c）
=> md 0xa4100000 4
a4100000: 7fbffddb fff8ffaf f7ffdfff 7ffdd7ed   ← DBI 窗口：浮空数据
（同样的地址再读一次，值又不一样：fffffddb fff8ffab f7ffdfff 7ffdf7ec）
```

**判读**：

1. 32 位窗口（APB `0xfe190000`、config `0xf4000000`）都正常 → U-Boot 的
   `pci enum` / 枚举成功正是走 `config` 窗口，**它从未证明 `0xa41000000` 可读**。
2. 高位 DBI 窗口读回**每次都不一样的浮空值** → 该地址在**我们这块板**上没有
   被任何从设备解码。这与我们内核的 DECERR（`DFSC=0x10`）是同一件事的两种表现。
3. 参考板同一地址能读出 `0x35881d87` → **两块同型号板子在"高位 PCIe 段是否
   解码"这一层就不同**。
4. 先前的"U-Boot 能读 DBI 所以硬件没问题"因此**作废**；反过来，这解释了我们
   为什么把 CRU/PHP_GRF/PMU/复位/客户端模式全部对齐也修不好——问题不在驱动。

高位 PCIe 段（`0x9_0000_0000`–`0xa_ffff_ffff`）的解码在 rk3588 上属于**安全/
地址映射配置**（ATF 或 U-Boot 早期初始化），内核侧改寄存器改不动它。

### 下一步（按优先级）


0. **先在两块板上做同一个 U-Boot 对照**（最省事、最能定性）：
   在参考板的 U-Boot 提示符下执行 `md 0xa4100000 4`。
   * 若它读出 `35881d87` → 差异在 **bootloader/ATF**（谁把高位段打开），
     需要对比两块板的 U-Boot/ATF 版本与 PCIe 相关初始化。
   * 若它同样是浮空值 → 说明参考板能读是**它的内核**做了什么（而 U-Boot 不做），
     那就回到驱动侧继续找。
   顺带把两块板的 `version`（U-Boot）与 ATF 版本（`strings` BL31 或
   `cat /sys/class/...`）记下来对比。

1. **高位 PCIe 段的 CPU 解码**（唯一剩余阻塞）。已排除：链路状态、时钟、
   复位、PD_PCIE、DT 解析、PHP_GRF、gate 掩码 `0x440` 邻居、
   `pcie-apb` 客户端配置。剩下最值得做的两件事：

   **优先项已改为：让 combphy 做出 `phy_calibrate()`**，把
   `combphy 0x28/0x2c/0x30/0x34` 收敛到工作 Linux 的
   `0x90 / 0x43 / 0x88 / 0x56`（我们现在是 `0x93 / 0x02 / 0x08 / 0x59`）。
   BSP 源码里直接抄实现（`drivers/phy/rockchip/`），不要猜。
   已核对过、**不再是**怀疑对象：复位线 id（板上 DTB 的
   `resets=<&cru 0x211 0x220>` 与 reset-names `pcie periph` 都与我们一致）、
   时钟、`reg`、`phys`、PHP_GRF、PMU、GENERAL_CON。原待查清单保留如下：

   - **核对板上 DTB 的 `resets` 属性**（参考板 `/proc/device-tree/pcie@fe190000/resets`）。
     我们只核对了 `reg`/`clocks`/`phys`，**没有**核对 reset id。若板上用的是
     `SRST_P_PCIEn` 的另一条线（而不是我们表里的 `SRST_PCIE4_POWER_UP` +
     `SRST_P_PCIE4`），DBI 从设备就会一直处于软复位 → 表现正是"窗口没路由"。
     **这是当前最优先的一项，一次串口读取即可判定。**
   - dump `SYS_GRF`（0xfd58c000）与 PMU 的 NOC 相关寄存器，找"高位 PCIe 段
     force-enable"位。
   - 顺带确认 `aclk_pcie_root`（0x880 bit6）与 `aclk_mmu_pcie` 在运行——它们
     是 Linux 侧 `power-domain@34` 的 consumer，我们靠 `CLK_IS_CRITICAL` 兜住，
     值得显式确认一次。

2. 无论如何都值得同时验证的一件小事：把 DBI 的映射从 4 MiB 降到 4 KiB
   （`probe_dbi_page` 已就绪）再读一次，排除"大映射被拆页/块映射搞坏"这种
   低概率但便宜的可能。
2. 上述打通后：`setup_rc` / iATU / `scan_and_log` 才有意义（代码已就绪）。
3. `PD_PHP`（PWR_CON bit5）在我们和 Linux 里都是 0，暂不作为怀疑对象。

### 里程碑记录（避免重复劳动）

| 修复 | 结果 |
|---|---|
| 客户端 RC 模式（general-con bits[2:0]=0b100，在首次 DBI 访问之前） | LTSSM 开始从 0 变化 |
| combphy 三路时钟（refclk/apbclk/phpclk） | PHY 寄存器恢复可读 |
| PD_PCIE 用 0x150 + 正确极性 | `PWR_CON` bit7=1、repair bit22=0，与参考一致 |
| 复位 assert→1µs→deassert 脉冲 | 与参考一致（非关键） |
| **以上合并** | **链路训练成功：0x2 → 0x00030011 (L0)** |

---

## 6. 做 harness 时要固化的教训

这一轮把 PCIe 打通前后踩的坑，按"能不能写进工具"分类。凡是可以机械化的，做成
harness 的检查项就能省掉整轮真机周期。

### 6.1 临时诊断不许改共享默认值（本轮最大教训）

**现象**：DBI 窗口无论换 `nGnRE`、`Normal-NC` 还是 `nGnRnE` 都失败，看起来
"属性无关"，于是被当成"硬件问题"追了很多轮。

**真相**：一次实验往 `memspace::iomap_device` 里加了"`paddr >= 4 GiB` 就用
`UNCACHED`"，实验结束没撤。`0xa41000000` 是全内核**唯一** >4 GiB 的映射，
所以它一直是 Normal-NC；后续所有属性对比**对被测窗口根本没生效**。
撤掉、统一成 `DEVICE` 后立刻通：

```
pcie-dw: dbi alive, vendor 0x1d87 device 0x3588
pcie-dw: link up, 1 lane(s)
pcie-dw: bus 0 dev 00.0 vendor 0x1d87 device 0x3588
```

**harness 化**：

- 任何"实验性默认值"必须走显式开关（编译期 feature / 运行时参数），禁止直接改
  共享函数里的分支；
- CI / harness 加一条 grep 检查：`iomap`/`map_linear` 之类共享路径里不许出现
  以物理地址阈值切换属性的分支；
- 每次改属性类实验后，**先 dump 一次 PTE** 确认实验真的作用到了目标窗口，
  再开始解读结果（否则整轮数据作废）。

### 6.2 MMIO 映射后立刻 dump PTE（新加的常规手段）

在 `iomap_device` 里对目标 VA 调 `page_table().query_entry(va)`，打印
`paddr / flags / page_size`：

```
iomap: pte VA 0xffff8000201bd000: paddr 0xa41000000 (asked 0xa41000000) Size4K
```

一行就能把"地址算错 / 属性给错 / 被拆成大页"三种怀疑一次否掉。建议 harness 把
它做成 `--verbose` 的默认输出，驱动侧不用各自加。

### 6.3 两种失败模式 + 一个"存活探针"

| 现象 | 含义 |
|---|---|
| `ESR=0x96000010`，DFSC `0x10`（synchronous external abort） | 总线返回 DECERR：地址没被解码，**或**访问属性被互连拒绝 |
| 静默 stall（无 panic、无 abort、日志停住） | 事务被接收但永不完成：目标在，但没时钟/在复位 |

**存活探针**（本轮找到的实用判据）：区间内某个控制器的 `pcie-apb` 客户端块
若 **整块读回全 0**，说明该控制器没被上电/开时钟，此时**绝不能去碰 DBI**，
一碰就 stall。fe190000 是活的（`0x18=0x7737`、`0x20=0x177f`），fe150000 全 0
→ 后者直接跳过，不再挂死整机。

### 6.4 参考板上的 U-Boot 是免费 oracle

打断 autoboot 后：

- `md <pa>` 看到的是 **MMU 关闭**（强序 nGnRnE）时的原始响应，用来判断"这个
  地址到底有没有从设备"；
- `pci enum` / `pci` 能确认控制器与设备在位（我们板子枚举出 2×RTL8125）；
- 注意：**枚举成功不等于 DBI 可读**——`pci` 走的是 32 位 `config` 窗口。

代价提醒：在参考板用 `dd if=/dev/mem` 读未解码地址会**把整机挂住**
（`panic_on_oops=0` 也救不了总线 stall）。改用 python + `mmap`，失败时是
SIGBUS（进程死、内核活）。

### 6.5 寄存器语义以工作实例为准，不要抄注释

- `PMU 0x290` 的 repair_status：注释写 "1 = power on"，但工作 Linux 上是 **0**；
  `PWR_CON(0x150)` bit7 才是 1。照注释写就会像我们一样"以为已上电"，实际没上。
- `PMU_BUS_IDLE_REQ` 的 `req_mask == 0` 表示该域**不参与** idle 握手，别硬套。
- 状态寄存器**可能跨热重启保留**：早期把残留的 `0x130011` 当成"链路已训练"，
  白追了好几轮。判断链路要看**训练过程中的变化**（`0x2 → 0x30011`），
  而不是某个瞬时快照。

### 6.6 复位：只 deassert，不要脉冲

两份参考驱动（BSP 与 mainline）都是 `reset_control_deassert`，不先 assert。
我们加了 assert→1µs→deassert 的脉冲后，LTSSM 从 `0x00130011` 变成
`0x00030011`（丢 bit20），即**脉冲把控制器搞差了**。去掉后与 Linux 逐位一致。

### 6.7 排查顺序建议（给 harness 的默认剧本）

1. 打印 DT 解析结果（`reg`/`clocks`/`resets`/`phys`/`ranges`），与板上 `/proc/device-tree` 对照；
2. 由参考板 `/dev/mem` 取一份寄存器基线（CRU 门控、PHP_GRF、PMU、APB 客户端）；
3. 按控制器逐个：phy → 复位 deassert → 时钟 → **APB 存活探针** → PTE dump → DBI 读；
4. 每步都打一行可机器比对的日志（`key = value`），让 diff 代替人眼；
5. 任何一步失败就**跳过该控制器**并继续，绝不因为一个没上电的控制器挂住整机。
