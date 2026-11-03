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

### 4.1.2 卡点的精确签名：**启动瞬间能发，约 1.3 秒后发不动**

用探针把"不消费"缩成了一个可复现的时间签名：

```
prime-1 own 0x00000000 intr 0x84     # 启动后立刻发 1 帧(60B) → 被消费、TX_OK
prime-2 own 0x00000000 intr 0x85     # 紧接着第 2 帧            → 也被消费
tx[0]   desc 0xb000002a -> 0xb000003c reclaimed false   # 1.3s 后 ping 的帧 → 不动
```

对照实验结论（每条都实测过）：

| 变量 | 结果 |
|---|---|
| 帧长 42 → 补到 `ETH_ZLEN`(60) | 描述符长度对了(`0x3c`)，但仍不消费 |
| 环地址 `0x203000` → `0xe8000000` | 无变化（低 DRAM 可达，地址假设彻底排除） |
| 一次 arm 4 个连续描述符 | 4 个全不消费 |
| 一次 arm 1~2 个并**立刻**踢门铃 | **全部消费** |
| 门铃后清 `DESC_UNAVAILABLE` + 重写 `ChipCmd` | 无变化 |
| 每帧发送前无条件重写 `ChipCmd` | 无变化 |
| 关 PCIe ASPM（`LNKCTL[1:0]`） | 这两位本来就是 0，未打印 |
| 关芯片低功耗（`OCP 0xe092/e094` + CSI L1 进入延迟 + ZRXDC） | 已生效(`csi pcie_ctrl 0x13ffff00`)，仍不消费 |

所以：**不是描述符格式、不是地址、不是门铃、不是掩码，而是设备在空闲约 1 秒后
停住了发送引擎**，而寄存器读取照常（BAR 访问会唤醒链路，DMA 不会）。

已知主线与 vendor 都在做、我们**还没做**的两件相关事情（`make ldp-sequence`
会持续把它们列出来）：

- `start.irq-enable`：参考实现用中断驱动 TX 完成（`rtl_irq_enable`），我们纯轮询。
- `start.ephy-init` / `start.phy-config`：EPHY 表与 PHY 调优。参考板的 vendor 驱动
  做了它们；U-Boot 的 r8169 不做也能发——但 U-Boot 的发送是**紧接着初始化**发生的，
  恰好落在"能发"的时间窗内，所以它不能证明"不做 EPHY 也能长期发"。

补上 `start.exit-l1`（`OCP 0xc0ac |= 0x1f80`：L1 退出事件，含 bit10 `txdma_poll`）
并复测：`l1 exit events 0x1f82` 已生效，单帧仍不被消费。

**顺带发现一个更严重的移植 bug**：`mac_ocp_modify` 我写成
`(data & ~mask) | (set & mask)`，而上游是 `(data & ~mask) | set`（`set` 不与
mask）。于是所有 `mask = 0x0000` 的**置位行全部变成空操作** —— 包括
`0xc0ac |= 0x1f80`、`0xc0b4 |= 0x0c`、`0xe860 |= 0x80` 三行。代码里"表抄全了"，
实际一个都没写进芯片。已修，并把这条写进 skill §6.5（辅助函数语义要逐字转录）。

### 4.1.2b 差分结论：**不是芯片特性**（2026-11-03 补）

参考板插好网线后，vendor `r8125` 驱动在**刚 up、空闲 30 s、空闲 60 s** 三个时间点
ping 跳板机全部 0% 丢包。所以"空闲后发不出去"不是 8125 的固有行为，
**是我们的驱动缺少某一步**。

另外把参考板工作态与我们逐项对齐后（都写进驱动里，读回一致）：

```
rxcfg 0x40c20c0e   txcfg 0x67100f40   cfg2 0x1d   cfg5 0x02
```

其中 `cfg2/cfg5` 的电源位**刻意保持关闭**：按参考板原值（`0x9d/0x03`，即端点
自己开启 ASPM/ClkReq）写入后，连 RX 都出现 overflow（`intr 0x0011`），说明
"照抄工作实例的全部寄存器值"里含**在当前缺 L1 机制时有害**的项——这条也记进
skill：对拍只能定位差异，不能无脑照抄。

尝试过但**不可用**的取证手段：参考板 `/dev/mem` 读 DRAM 被
`STRICT_DEVMEM`/权限挡住（`PermissionError`），拿不到它 TX 描述符的原始内容；
要用这条路得在参考板上编译一个内核模块，暂不做。

### 4.1.2c 逐项 bisect 的结果（2026-11-03 晚）

把参考板的寄存器值一项项抄进驱动并复测，得到两个反向结论：

| 改动 | 结果 |
|---|---|
| `TxConfig` 抄参考值 `0x67100f40`（含 bit6） | **倒退**：门铃不再被消费（`txpoll 0x0001`） |
| `TxConfig` 用计算值（bit6=0，读回 `0x67100f00`） | 门铃恢复被消费（`txpoll 0x0000`），`intr 0x0081` |
| `RxConfig` 抄参考值 `0x40c20c0e` | 无变化（保留，与参考一致） |
| `Config2/5` 抄参考值（端点自开 ASPM） | **倒退**：RX 出现 overflow（`intr 0x0011`） |
| EPHY 表（`e_info_8125b` 六行） | 无变化（保留，参考一致） |

所以"照抄工作实例的寄存器值"不能无脑做：`TxConfig` bit6 与 `Config2/5` 的电源位
在**对方有配套 L1 机制**时成立，在我们这边会直接让队列停摆。已写进 skill。

### 4.1.2d 当前最强假设：DMA 一致性（缓存）

现在每次都是：门铃被消费、`intr` 里出现 `TX_DESC_UNAVAILABLE`，而我们刚写的
描述符 OWN 位仍为 1。也就是 **MAC 扫了环，看不到我们写的内容**；而"启动瞬间
（直接 `arm_whole_tx_ring` 256 个）却能全部消费"。时间相关性 + 读写双向都表现为
"设备看到的是旧内存"，最像**缓存一致性**问题：

- `kdma` 的 `allocate_dma_memory` 把页重映射成 `READ|WRITE|UNCACHED|SHARED`
  （`update_flags` → `DmaPageTableIf::protect`）；若该重映射没有真正生效（或
  TLB 未失效），内存仍是 cacheable；
- `driver_host` 只在 `dma.coherent == false` 时才做 `flush_cache`/`invalidate_cache`
  （见 `drivers/host/src/lib.rs`），而我们的 DMA 视图是 "identity 64-bit coherent"，
  于是**一次 cache 维护都不会发生**；
- 内核目前**没有**任何 cache 维护原语（`arch/khal`、`arch/kcpu` 里搜不到），
  所以一旦确实是非一致平台，驱动侧无法自行 flush。

### 4.1.2e DMA 一致性假设**已排除**，问题回到"引擎停住"

`driver_host` 的 DMA provider 现在会在每次 coherent 分配后打印页表实际属性
（`memspace::kernel_layout().page_table().query_entry()`）：

```
dma: coherent 0x1000B cpu 0xffff000000203000 bus 0x203000
      pte paddr 0x203000 READ | WRITE | UNCACHED Size4K      # TX 环
dma: coherent 0x1000B cpu 0xffff000000204000 bus 0x204000
      pte paddr 0x204000 READ | WRITE | UNCACHED Size4K      # RX 环
dma: coherent 0x80000B cpu 0xffff000003400000 bus 0x3400000
      pte paddr 0x3400000 READ | WRITE | UNCACHED Size4K     # slots
```

映射是 **UNCACHED 且恒等**（paddr == bus），所以描述符写**确实**到了 DRAM，
缓存不是原因。

那"MAC 看不到 OWN"就只能是**它没有在读**。加环消费计数后确认：

```
ring consumption tx_done 255/256 rx_done 0/256
```

- `tx_done 255` 只是初始化时 255 个 `opts1=0` 的占位（判据 `OWN==0` 也把"从未
  arm"算成 done —— 这个判据本身要小心）；
- **`rx_done 0`**：RX 环 256 个 armed 描述符**一个都没被消费**；
- 也就是说**收发两个引擎都停着**，`intr` 里的 `RX_OK` 是早期残留（我们只 ack 过
  `0x80`，其它状态位一直挂着）。

**新签名**：初始化后约 1 秒内引擎在跑（`arm_whole_tx_ring` 时 256 个全被消费、
`TX_OK`），之后**两个引擎都不再取描述符**，而寄存器读写照常、MAC 侧 link 仍报 up。
参考板同芯片空闲 60 秒仍正常，所以是**我们少了一个让引擎持续运行的配置**，
最可能在 `start.phy-config`（`rtl8125b_hw_phy_config`）或 **link 状态机**
（autoneg 完成会改变 speed/duplex，而我们对 MAC 的启动配置是在 autoneg 完成前做的）。

### 4.1.2f 又排除两条：link 状态变化与"引擎卡死需 stop/start"

| 假设 | 实测 | 结论 |
|---|---|---|
| MAC 启动配置是在 autoneg 完成前做的，协商完成后失效 | `phy at start` 与 `phy at tx` 完全相同（`bmcr 0x1040 bmsr 0x79ad phy_status 0x8b`，autoneg 完成、1000BASE-T full） | 排除 |
| 引擎卡在只有 stop/start 能清的状态 | 发送前 `ChipCmd=0` → arm → `ChipCmd=RxEnb\|TxEnb` → 踢门铃，仍不消费 | 排除 |

到这里，**"寄存器配置"这一层能想到的都排掉了**。剩下只有两个结构性差异，都在
`make ldp-sequence` 的未移植清单里：

1. `start.phy-config`（`rtl8125b_hw_phy_config` + `configure_default_copper_autoneg`）
   —— 参考板 vendor 驱动做了、我们完全没做；
2. `start.irq-enable` —— 参考实现是**中断驱动**，我们是纯轮询；本板 INTx 未路由
   （`interrupt line = 0xff`），要做必须先有 MSI-X。

**取证建议（若继续）**：在参考板上装一个读 `0xe4010000` 描述符的小模块，对比其
TX 描述符 opts1/opts2 的**原始字节**与我们写的是否存在我们没意识到的差异；
`/dev/mem` 走不通（`STRICT_DEVMEM`），见 board-automation §5.8。

### 4.1.2g PHY 配置与 link 状态机已补，仍不消费

- **`start.phy-config` 已移植**（`hw_phy_config_8125b`：3 条分页行 + 0x0b87 直写块 +
  10 条 `phy_param` + 2 条分页行 + 铜口 1000BASE-T 广播/重启 autoneg）。
  生效证据：`phy config applied, bmcr 0x1040 ctrl1000 0x0200`。
- 重启 autoneg 让链路瞬断，于是第一次发送时 `IntrStatus` 里出现
  **`LINK_CHANGE`(bit5)** —— 这暴露了我们的另一个真实缺失：**没有 link 状态机**。
  参考驱动在 link 变化后重配 MAC（`rtl_task → rtl8169_check_link_status →
  rtl_hw_start`）。已实现 `handle_link_change`：ack 事件 → 读 `phy_status` →
  重跑 `program_ring_bases` + `arm_receive_ring` + `program_engine_config`。
  实测触发正常：`link change handled, phy_status 0x93 link_up true -> reprogramming`。
- 但重配之后**描述符仍不被消费**。

至此参考实现 `start` 路径上"我们没做"的只剩 `start.irq-enable` 与
`start.tally-counter`（`make ldp-sequence` 会列）。也就是说：
**寄存器层面的初始化序列基本对齐完了**，剩下的结构性差异是**中断驱动 vs 纯轮询**。

`phy_status` 在 PHY 配置前后发生了变化（`0x8b` → `0x93`，bit3/bit4 不同，疑似
speed 编码变化），这点值得在下一步一起确认——如果协商结果与我们给 MAC 的配置
不匹配，TX 也可能停摆。

### 4.1.2h `phy_status` 与"TX 读指针"线索

- **不再无谓打断链路**：`hw_phy_config_8125b` 里的 autoneg 重启现在只在
  "1000BASE-T full 未广播 **且** 链路 down" 时才做。实测之前是它在**链路已 up 时
  重启协商**，把链路打断一次（`bmsr 0x7989` → link=0），再恢复。修好之后
  `phy at start bmsr 0x79ad`（link up、autoneg complete、1000BASE-T）全程稳定。
  **但 TX 仍不消费**，所以"link bounce"不是根因（修法本身保留，它是对的）。
- **`phy_status` 编码**：`0x8b`(bit3) / `0x93`(bit4) 两态都出现过；bit0=全双工、
  bit1=LINK_UP、bit7 常置。结合 `bmsr 0x79ad`（1000BASE-T）看，MAC 侧看到的
  speed 与 PHY 协商结果并无矛盾，所以这条线索**降级**。
- **TX 读指针线索（当前最强）**：启动瞬间把**整条环**（256 个）填成 OWN 后踢一次
  门铃，**256 个全被消费**且 `intr` 只有 `TX_OK`（连 `DESC_UNAVAIL` 都没有）；
  而 5 秒后 ping 写**单个**描述符到 index 0 就不被消费、也不再报 `DESC_UNAVAIL`。
  这与"MAC 的取指指针停在 k≠0（空环上它会跳过非 OWN 的描述符），而我们的
  `tx_next` 从 0 开始"完全一致。
  实测过但**无效**的修法：使能引擎后重写 `TxDescStartAddr{Low,High}` 复位指针
  （现象不变）。下一步应当**用驱动自己的状态机**在 t+0 发一帧（而不是旁路
  `send` 直接 arm），再在 t+5 s 发第二帧，看第二帧是否被消费——这能干净地把
  "指针不同步"与"引擎停摆"分开，之前的 prime 探针绕过状态机，结果不可解释。

### 4.1.2i tgoskits 是**同板已验证**的参考（不要丢）

`/workspace/tgoskits` 的 `drivers/net/realtek-rtl8125` **在 OrangePi 5 Plus 上做过
board bringup**（retrospective 记录的 PR #404），且被 Axvisor 当管理网卡使用
（`docs/docs/architecture/axvisor/guest-console.md` 明确写了它驱动 OPi5+ 的
RTL8125）。也就是说：**同一块板、同一颗芯片，有一份能工作的 Rust 实现**。

已经用它补齐/纠正的点：

| 项 | 来源 |
|---|---|
| CSI 窗口（`CSIDR 0x64` / `CSIAR 0x68`）+ `CSI_PCIE_CTRL` 的 ZRXDC + L1 进入延迟 | tgoskits |
| EPHY 表（`e_info_8125b` 六行，走 `EPHYAR 0x80`） | tgoskits / mainline |
| **中断偏移 `IntrMask 0x38` / `IntrStatus 0x3c`（32 位）** | tgoskits |
| `CplusCmd` 的 `PCI_DAC`/`PCIMulRW` 语义（本芯片只读 0） | tgoskits |
| `Config1` bit4 的真实语义 = **SPEED_DOWN**（空闲降速），不是无名低功耗位 | tgoskits |
| `RxConfig`/`TxConfig` 位段与取值（burst=7 / IFG=3 / fetch=8 / pause_slot） | tgoskits（与我们的推导一致） |
| **`RxConfig` 应用 `0x40000f00\|accept`，不是参考板 dump 的 `0x40c20c0e`** | tgoskits（本轮据此纠正） |

**它和我们剩下的结构性差异只有一个**：tgoskits **是中断驱动**（注册 IRQ source，
队列启动挂在 RX-ready + IRQ 路径上），我们是**纯轮询**；本板 INTx 未路由
（`interrupt line = 0xff`），所以要先有 MSI-X。

### 4.1.3 **拿到 vendor 权威源码了**（网络可用）

这台机器能访问外网，所以不必反汇编：

```bash
curl -sL -o r8125.tgz \
  https://codeload.github.com/awesometic/realtek-r8125-dkms/tar.gz/refs/heads/master
# → Realtek 官方 r8125 9.018.00-NAPI（板上跑的是 9.014.01）
# 已解到 /workspace/r8125-vendor-9.018.00，并登记进 data/references.json
```

**这一下把"权威源码"这条门禁真正落地了**：vendor 源码与 mainline r8169 在若干处
不同，之前按 r8169 抄的都可能是错的。

用 `ldp sequence extract`（顺带修好了它对**跨行函数签名**的处理）抽出
`rtl8125_hw_config` 的 **93 步**，逐项对照后确认/修正了这些：

| 项 | vendor 的真实做法 | 我们之前 |
|---|---|---|
| `TDFNR (0x57)` | **写 0x10**（一次门铃取 16 个 TX 描述符） | 从不写，芯片默认 **0x01** |
| `0xE63E` bits 5:4 | **置 `0b10`**（TX 队列配置）+ TX 队列数 bits 11:10 = 0 | 按 r8169 把它们**清掉** |
| `0xE052` | 置 bits 6,5；**清** bits 3,7 | 置了 bit3 |
| `0xD430` | `0x45F` | `0x47F` |
| `0xC0B4` bit0 | **先清后置**（MCU 使能 toggle） | 只置 bits 2,3 |
| `0xEB58` bit0 | **置**（"new tx desc format"） | 按 r8169 **清**（我们用 legacy 格式，保持 0） |
| `RxConfig` | 来自 `rtl_chip_info[].RCR_Cfg` = `Rx_Fetch_Number_8 \| RxCfg_pause_slot_en \| EnableInnerVlan \| EnableOuterVlan \| (RX_DMA_BURST_256 << shift)` = **`0x40c20c0e`** | 先抄参考板值（对），我上一轮又回退了（错，已说明） |

还发现 vendor 的 **`rtl8125_hw_start`（`ChipCmd = TxEnb\|RxEnb`）是在 link-up 补丁里
调用的**：`rtl8125_link_on_patch = hw_config(整套重配) → 速率补丁 → hw_start`，
而我们只在 probe 里使能一次。

补上 TDFNR 与上述 OCP 行后复测：**TX 仍不消费**。

### 4.1.4 序列门禁现在跑在 vendor 源码上（41 步 / 9 未移植）

`data/sequences/rtl8125.toml` 的 `authoritative` 已改成 vendor 源码，并支持
**跨 crate 锚点**（`crate_paths = ["drivers/bus/pci"]`）——因为 PCIe 配置调优属于
bus 层，不在驱动 crate 里。当前门禁输出：

```
步骤 41：anchored 26 / waived 6 / unported 9
未移植：start.irq-enable, start.tally-counter,
        cfg.rx-packet-filter-off, cfg.tally-addr, cfg.extend-tally,
        cfg.tcam-enable, cfg.l1-l0s-latency, cfg.ephy-config-8125b,
        cfg.phy-config-full
```

**已按 vendor 做法补的一项**：`cfg.mrrs` —— 枚举时把每个 function 的
`PCI config 0x79 bits[6:4]`（Max Read Request Size）从固件留下的 **512B** 提到
vendor 使用的 **2048B**：

```
pci: 10ec:8125 device control 0x2010 (max payload 128B, max read request 512B)
pci: 10ec:8125 max read request raised to 2048B (device control 0x4010)
```

TX 仍不消费，所以 MRRS 也不是根因。

### 4.1.5 按 tgoskits（同板已验证）对齐，并修掉一个真实的屏障 bug

"tgoskits 是中断驱动所以没有参考价值"这个推断**不成立**：中断只影响"如何得知完成"，
不影响寄存器/队列配置的正确性。这一轮按它对齐后补了三处：

1. **OCP 行改回 tgoskits 的取值**（它与 vendor 在这几行上有分歧，但它是同板实测能跑的）：
   `0xe63e` 清 bits 5:4、`0xe052` 置 0x68/清 bit7、`0xd430 = 0x47f`；
   另外补上它的 `0xe040 |= bits 1:0`（我们照 r8169 只清了 bit12）。
2. **多播哈希过滤 `MAR0/MAR4 = 0xffffffff`**：复位后这两个寄存器是全 0，
   即使 accept 位写了多播，多播组仍会被丢掉（IPv6 ND、mDNS 都会受影响）。
3. **发送路径缺一个写屏障**（真 bug）：我们的顺序是
   `fence(Release) → 写 OWN → 门铃`，而 tgoskits 是
   `写描述符 → fence → 写 OWN → **wmb()** → 门铃`。
   DMA 内存是 Normal-NC（不是 Device），CPU 可以重排，**门铃先到**时 MAC 扫到的是
   OWN=0 的环，正好报 `TX_DESC_UNAVAILABLE`。已在门铃前补 `barrier_release()`。

补完这三处复测：`intr 0x0081`（RX_OK + DESC_UNAVAIL）、描述符仍不被消费。
MAK 校验：`make ldp-sequence` 绿（41 步 / 26 anchored / 9 unported）。

### 4.1.6 "逐字脚本"这一步做完了，并排除了最后三个假说

按 vendor `hw_config` 补齐了我们缺的项（顺序照抄）：`disable_rx_packet_filter`
（置引擎前先清全部 accept 位）、`0xC0B6 &= BIT0`、`extend_tally`
（`OCP 0xea84 |= 0x3`）、`tally_counter_addr_fill`（把 CounterAddr 指向我们的
slot 区，顺带是个纯设备→内存的 DMA 烟囱）、`disable_double_vlan`
（`0x1000 = 0`）、`set_l1_l0s_entry_latency`（**CSI 0x70c bits[29:24] = 0x27**，
与 r8169 的 `0x719` 字节写法不同）：

```
rtl8125: vendor hw_config extras applied, csi 0x70c 0x27ffff00
```

同时确认 **`rtl8125_enable_tcam` 就是 `0x382 = 0x221B`（我们早就做了）**，
`set_rx_crc_drop` 只对 CFG_METHOD_8..16 生效（不是本芯片）。

**又排除了三个假说**（都有实测）：

| 假说 | 实测 | 结论 |
|---|---|---|
| MAC 取指指针停在环尾（RingEnd 那个） | 同时 arm index 0 与 index 255，两个都保持 OWN=1 | 排除 |
| 引擎按 TDFNR=16 成批取，批内不全 owned 就整批放弃 | arm 前 16 个（正好一批）→ 0/16 被消费 | 排除 |
| MRRS 设 2048 超过 RC 能力导致读被 UR 终止 | 撤掉 MRRS 提升（回到 512）→ 现象不变 | 排除 |

**唯一的正证据仍然只有一条**：启动时把**整环 256 个**都 arm 后踢门铃 →
256 个全被消费、`TX_OK` 置位；而 arm 1 个、2 个、16 个（无论放 index 0 还是 255）
都不被消费。这条在今天的代码状态下**仍可复现**（`intr` 里能看到 `TX_OK`），
所以 TX 通路本身是好的，缺的是"单帧提交为什么不被看见"这一环。

## 5. 这次调试的方法论教训（比结论更值钱）

TX 卡了整整一轮会话、试了十几轮假设才通。事后看，**每一条弯路都有共同的形状**，
写下来给下一次（也包括 USB/MAC/PHY 这些同构的活）。

### 5.1 先问"这份证据是从哪个状态取的"，再问"哪个假设成立"

本次最贵的错误：把**probe 时的 init** 和**几秒后的运行态**当成同一份设备状态来对比。
它们不是 —— probe 时链路还没协商完，那次 init 装的 MAC 状态**永远不会变正确**。
于是：

- 所有"能发"的证据都出现在 **init 刚跑完的瞬间**；
- 所有"不能发"的证据都出现在 **probe init 之后很久**；
- 中间的每一次对比都在拿两个不同的状态互相解释，于是"上下文/CPU"这种假象才成立
  （CPU 号只是"init 之后多久"的代理变量）。

**规则**：任何 A/B 之前先确认两侧的**设备状态是同一份**。最省事的做法是**在同一次
启动、同一个函数里、紧邻地**做两次不同实现的实验（本次的"交叉实验"），而不是跨时间点
对比日志。

### 5.2 参考驱动里的 `init/start` 是**在 open 时被调用的**，不是 probe

Linux 的 `ndo_open` / vendor 的 `rtl8125_open` 在**接口 up 之后**才跑
`hw_init → hw_reset → hw_phy_config → hw_config → hw_start`，此时 PHY 通常已经协商完。
我们的 `probe` 在**枚举时**就跑同一段，等价于"给一个还在协商的链路装 MAC"。
移植平台驱动时的对应做法：**probe 只取资源/读身份，真正的 start 放到"链路 up 之后"
（polled 或 link-change 事件）**。

### 5.3 一次只测一个变量，并且让测量自己会说话

有效的做法：
- **地址扫描**：把同一个动作在多个地址上做一遍，看哪几个被接受（一眼区分"地址相关"
  与"动作无关"）；
- **窗口测量**：同一动作在 +0/+1/…/+5 s 各做一次，直接证伪"时效"类假设；
- **交叉实验**：A 的初始化 + B 的提交，把两个变量分开。

无效的做法（本次浪费最多的）：读一段参考代码 → 猜一个寄存器 → 改一行 → 上一轮板。
**每一轮板都应该能证伪一个假设**，而不是"看看会不会好"。

### 5.4 寄存器"读得到"不等于"配置生效"；"写进去"不等于"语义正确"

三个真实案例：
- `Cfg9346` 写保护：描述符基址、Rx/TxConfig 都在 latch 后面，**未解锁的写会被静默丢弃**
  （地址扫描里三块"失败"其实是基址写没生效）；
- `mac_ocp_modify` 的 `set` **不与 mask**（上游 `(data & ~mask) | set`），抄成
  `(set & mask)` 会让所有 `mask=0` 的置位行变成空操作；
- `TxConfig` 的 IFG 字段只有 **2 位**（[25:24]），bit26 起是只读版本号 —— 这也正是
  `rtl_chip_infos` 用 `0x7cf` 掩码的原因。

**规则**：对"读改写"类的辅助函数，**语义要逐字转录**（参数含义一起抄）；对受保护的
寄存器，**写之后必须读回并断言**。

### 5.5 对拍工作实例能给"差异"，但不能给"配方"

参考板（vendor `r8125`）和 tgoskits 都是能用的一手资料，但它们各自的值**只在它自己的
上下文里成立**：

- 抄参考板 `TxConfig=0x67100f40`（bit6=1）→ 门铃不再被消费；
- 抄参考板 `Config2/5`（端点自开 ASPM）→ RX 出现 overflow；
- 抄 tgoskits 的 `RxConfig`（burst=7）→ 与 vendor 的 `RCR_Cfg`（burst=256 → 4）不同，
  vendor 的才是本芯片表值。

**规则**：差异是线索，改完必须单独验证；一次只合入一项，并写下"如果它有效，
日志会多出什么"。

### 5.6 让工具替你记仇

本次新增的 `ldp sequence`（序列门禁）与 `data/observations/` 就是为了让"漏一步"
和"读错偏移"这类错误**下次自动失败**。每修好一个 bug，至少落一处数据：

| bug | 落到哪 |
|---|---|
| 缺 `rtl_hw_init_8125` | `sequences/rtl8125.toml` 的 `init.*` 步骤 + 锚点 |
| `IntrStatus` 读 `0x3e` | `observations/rtl8125.toml` 的 `changes_across_tx` 事实 |
| `mac_ocp_modify` 语义错 | skill §6.5 的案例 |
| 描述符基址受 latch 保护 | 本节 5.4 |
| init 必须在链路 up 之后 | 本节 5.2 + 下面的序列步骤 |

## 4.3 收口：**网卡通了**（ping 4/5 通），根因是"probe 时的初始化无效，链路稳定后要重跑一次"

```
64 bytes from 172.30.193.53: icmp_seq=1 ttl=63 time=20.5 ms
64 bytes from 172.30.193.53: icmp_seq=2 ttl=63 time=5.39 ms
64 bytes from 172.30.193.53: icmp_seq=3 ttl=63 time=4.84 ms
64 bytes from 172.30.193.53: icmp_seq=4 ttl=63 time=1005 ms
5 packets transmitted, 4 received, 20% packet loss
```

## USB bring-up（M1：DWC3 + xHCI，High-Speed）

### 已落地

- `drivers/bus/usb`（package `usb`）：描述符线格式、`Hcd` 传输契约、枚举状态机
  （address 0 先读 8 字节拿 `bMaxPacketSize0` → `SET_ADDRESS` → 全量描述符 →
  配置 → `SET_CONFIGURATION`）、字符串描述符（UTF-16LE→ASCII）。
- `drivers/drv/usb`（package `drv-usb`）：`xhci`（capability 解析、halt/HCRST、
  DCBAA/CRCR/ERST/ERDP、单 interrupter、端口扫描）+ `dwc3`（父节点 clocks、
  `usb3-otg` reset、USB2 PHY、core 软复位、`GCTL.PrtCapDir=host`、
  `GUSB2PHYCFG`）。
- 绑定 `snps,dwc3`（子节点）；父节点的 `clocks` 用 `parent_property` 读。

### 真机进展（4 次上板，逐次收敛）

| 现象 | 根因 | 状态 |
|---|---|---|
| 驱动完全不 probe | 绑定父节点，而父节点没有 `reg`（只有 `ranges`） | 改绑 `snps,dwc3` 子节点 ✅ |
| `device 420 still deferred on phy:426` | DWC3 的 `phys` 引用 **`otg-port` 子节点** 的 phandle，而 usb2phy provider 发布的是外层 `usb2-phy@X` 的 phandle | 驱动侧先做 port→父节点映射（provider 应按 port 发布，已记为后续缺口）✅ |
| 控制台静默 83 s（无 panic） | `PD_USB` 未开：寄存器窗口首次访问直接挂总线，不产生 abort | 直接写 PMU 0x150 bit11 开域 ✅ |
| 首次寄存器读 `DataAbort`：`ESR=0x96000010`（DFSC 0x10 = 同步外部 abort，即总线 ERROR 响应） | **仍未解决**：PD 已开、`usb3-otg` 已 deassert（`status=false`）、三个时钟已 enabled（24M/24M/300M），块仍不解码 | 进行中 |

### 下一步（有明确判据）

1. **PMU 极性存疑**：我们的 0x150 读回 `0x00000811`（bit11 置位即认为"供电"），
   而 pm-domains.c 的 power-on 写的是 `pwr_w_mask`（= `BIT(11)<<16`，低半部分
   写 0）。这两种解释相反，必须用参考板实测定案：在**参考板 USB 工作态**下读
   PMU `0x150` 与 `0x1a4`（USB 域有 `mem_status BIT(3)`，我们的实现完全没碰
   内存供电位），与我们的值对比。
2. 若 PMU 值一致仍 abort，则查 `usb3-otg` reset 是否真的是这条线：读 CRU
   `SOFTRST_CON` 对应位（reset id 0x2a7 → `0xa00 + (0x2a7/16)*4`，bit `0x2a7%16`），
   并与参考板同寄存器对照。
3. 块能解码后，`GSNPSID` 应为 `0x5533_xxxx`；xHCI `HCIVERSION` 应 ≥ `0x0100`。

### 当前状态

| 方向 | 状态 | 证据 |
|---|---|---|
| **TX** | **可用** | `tx[n] len … reclaimed true`，每帧都被 MAC 消费 |
| **RX** | **可用** | `10 packets transmitted, 10 received, 0% packet loss`，连续两轮；RTT 均值 4.7–5.7 ms、最小 0.47 ms |

RX 的两个卡点，都不是"硬件没收包"：

1. **完成状态判错** —— 8125B 默认走 type-1 描述符环（只有开 RSS/PTP 才切
   type-3/4），判错只有 `RxRES(bit21)` 一位；参照位集合写掩码的人会把
   **每一个好帧**当错误丢掉。表现极具误导性：`0 received` 且 `rx[0]` 始终
   owned，看上去像 MAC 根本没往环里写。把描述符 `opts1` 与帧头一起打出来
   就一目了然（`rx[5] opts1 0x3800007b head 01:80:c2:00:00:00 …` 是好帧）。
2. **轮询模式缺接收节拍** —— 无中断路由时，knet 的 10 ms 定时器原本只在协议
   定时器到期才唤醒数据面，于是空闲期到达的回包要等下一次发送被唤醒才取走：
   RTT 出现整 1005 ms 尖峰、伴随约 20% 丢包。让该节拍每拍都通知数据面后
   归零。

### 定位过程（关键几步）

1. **交叉实验**：把 tgoskits 的 init 序列 + **我们自己的 `send` 提交写法** 放在一起跑 →
   `ab cross: own 0 intr 0x84` **成功**。⇒ 提交代码没问题，问题在初始化。
2. **窗口测量**：同一次 init 之后按 0/1/2/3/4/5 s 依次提交 → **全部成功**。⇒ 不是"init
   之后多久内有效"的时效问题。
3. 于是把 init 移到提交之前（`late_init`：链路 up 后在首次发送前**重跑整套启动序列**）
   → **`tx[0] ... reclaimed true`，帧真的发出去了**。

### 根因（一句话）

**probe 阶段（t≈15 s）跑启动序列时 PHY 还在协商链路**，那次 init 安装的 MAC 状态
**之后再也不会变正确**（在 probe 里连跑两遍也没用，因为两遍都在链路稳定之前）；
而链路稳定之后（t≈18 s+）用**同一段序列**重跑一次，立刻就能收发。

这也解释了为什么整个过程的"正证据"都出现在 init 刚跑完的时刻，而"负证据"都出现在
probe init 之后很久的时刻 —— 两者不是同一份设备状态。

### 现在的实现

`Rtl8125NetDevice::late_init()`：在**首次发送前**若链路已 up，则重跑
`tgoskits_start()`（复位 + MAC MCU/link-list + CPlusCmd + Rx/Tx 配置 + 合并块 +
EPHY 表 + 8125 common + PHY 配置 + arm RX + 环基址 + 使能引擎）。这是参考实现
link-up 状态机的**轮询版**（本板 INTx 未路由，中断版需要 MSI-X）。

剩余问题（下一轮）：**20% 丢包 + 偶发 1 s 延迟**，像是轮询模型的响应延迟/RX 重挂载
时机，而不是链路问题。

### 4.2 A/B 结论：**不是序列、不是寄存器、也不是时间，而是执行上下文**

在新分支 `xiaohui/rtl8125-tx-ab` 上做了一个最小 A/B：把 tgoskits 的
`init()` + `try_start_queues()` + `submit_buffer()` **按原顺序**在我们的驱动里重放
（同一块板、同一颗芯片、同一份 DMA 内存），只发 1 帧，然后记录 OWN 位与中断状态。

同一次启动里跑两次（完全相同的一段代码）：

```
[19.89] cpu 0:  ab: tgoskits sequence result: own 0  opts1 0x00000000  intr 0x00000084   ← 帧被取走并发送
[25.31] cpu 6:  ab: tgoskits sequence result: own 1  opts1 0xb000003c  intr 0x00000080   ← 完全不取
```

| 变量 | 是否成立 |
|---|---|
| 参考序列（vendor 93 步 / tgoskits 全套） | **否** —— 同一段代码两种结果 |
| 寄存器取值（含 TDFNR/MRRS/EPHY/PHY/MAR/OCP 全表） | 否 |
| 时间（"空闲 1 秒后失效"） | **否** —— 失败点可以是 t+5 s，也可以是 t+0.3 s，取决于调用点 |
| **执行上下文 / CPU** | **是** —— probe 上下文（CPU 0）成功，任务上下文（CPU 6）失败 |

这说明：**MAC 的取指通路本身是好的**（probe 里能取能发），一旦从任务上下文提交就
全灭。可能的方向（按可能性）：

1. **DMA 内存的 CPU 侧映射在多核下不一致**：`kdma` 用 `UNCACHED|SHARED` 重映射页，
   若 `protect()` 只对被调用的那个 CPU 做了 TLB 失效，其他核仍用旧的 **cacheable**
   映射访问同一物理页 → CPU 6 上写的描述符停在 cache 里，设备读 DRAM 读到 OWN=0。
   这与"probe 在 CPU 0（做过重映射的那个核）上能工作"完全吻合。
2. RC/DMA 侧与 CPU 亲和/互连电源域相关（可能性低于 1）。

**已排除的机制**（都在 A/B 里直接试验过，全部无效）：

| 机制 | 试验 | 结果 |
|---|---|---|
| 缓存脏行（写停在 cache 里） | 写 OWN 后对描述符地址发 `dc civac` + `dsb sy` | 无变化 |
| 每核 TLB 未同步 | 查 `flush_tlb_all_cpus` 实现：aarch64 走 inner-shareable 广播（`vaae1is`/`vmalle1is`），天然覆盖所有核 | 机制不成立 |
| 我们自己的 link-change 恢复把它搞坏 | 把 A/B 移到 `handle_link_change` **之前** | 仍失败 |
| 复位运行中的 MAC 需要先停引擎 | A/B 里加 `ChipCmd=0` + 等 FIFO 排空再复位 | 仍失败 |

**注意一个观测质量问题**：加上 `dc civac` 之后，probe 上下文的 A/B 结果从
`own 0 + intr 0x84(TX_OK)` 变成 `own 0 + opts1 0x1 + intr 0x80(无 TX_OK)` ——
`opts1=0x1` 不像一次真实发送的写回。也就是说 **"probe 上下文能发"这条正证据在
最新代码状态下已经不干净了**，下一步必须先把它重新测扎实（例如重复 5 次、
每次都记录 `TX_OK` 与回头看 RX 是否收到广播），再谈其它。

### 4.2.1 地址扫描：`kdma` 没问题，但只有 `tx_ring` 的基址真正生效

在 A/B 里做了一次**地址扫描**：把 TX 环基址依次指向本驱动四块不同的 coherent
分配，每块里 arm 一个描述符再踢门铃：

```
ab: address sweep tx_ring@0x203000:true  rx_ring@0x204000:false
                 tx_slots@0x3400000:false  rx_slots@0x3480000:false
ab: tgoskits sequence result: intr 0x84 / 0x85      # 含 TX_OK，是真发送
```

两条结论：

1. **`kdma` 的分配是可被设备读的**：`tx_ring@0x203000` 被接受并且真的发出了帧
   （`TX_OK`）。所以不是 DMA 内存/映射的问题，这条线可以关掉。
2. **后三块"失败"其实是基址写没生效**：扫描时四次写 `TxDescStartAddr{Low,High}`
   都**没有先解锁 `Cfg9346`**，于是只有早已生效的 `tx_ring` 基址是真的；arm 在
   其它缓冲里的描述符自然没人看。**描述符基址寄存器受 `Cfg9346` 写保护** —— 这一点
   值得记进 skill（我们的 `program_ring_bases` 是在解锁期间写的，所以生产路径没问题）。

顺带确认：把 A/B 的调用点全部移除、只跑纯生产路径 → TX 仍不消费；把
`handle_link_change`、`dc civac` 分别去掉也都不改变结果。所以
"扫描成功 vs 生产失败" 的差异还没定位到，已知它**不是** kdma、不是缓存、不是 TLB、
不是 link 状态机、不是序列、不是寄存器取值。

### 4.2.2 逐寄存器对比：两次提交的上下文**完全相同**

在扫描里"被接受的那次提交"和"生产路径的提交"前后各 dump 一次同样的字段，同一次
启动、相隔约 50 ms：

```
ab submit (sweep, accepted):  base 0x203000 desc opts1 0xb000003c opts2 0x0 addr 0x3400000
                              | cmd 0x0c intr 0x00000084 mask 0 tdfnr 0x10 txcfg 0x67100f00
send submit (production):     base 0x203000 desc opts1 0xb000003c opts2 0x0 addr 0x3400000
                              | cmd 0x0c intr 0x00000000 mask 0 tdfnr 0x10 txcfg 0x67100f00
```

**除 `IntrStatus`（历史值）之外逐字相同** —— 同样的基址、同样的描述符 16 字节、
同样的命令/掩码/TDFNR/TxConfig，一次被取走、一次被忽略。也就是说**提交动作本身没有
差异，差异在"之前跑了什么"**。

围绕这两次提交又排除了四种解释（都实测无效）：

| 试验 | 结果 |
|---|---|
| 去掉 `dump_registers()`（它读 0x64/0x68 CSI、0x80 EPHYAR、0xb0 OCPDR 等有读副作用的寄存器） | 仍失败 |
| 关闭 `handle_link_change` | 仍失败 |
| 每次提交前重写 TX 基址（门铃之前） | 仍失败 |
| 每次提交前重写 TX 基址（描述符之前，即扫描的顺序） | 仍失败 |

现在唯一已知的差别是**先后关系**：被接受的那次，紧跟在 A/B 自己的
`CmdReset → init → 重新写基址 → arm → 门铃` 之后；失败的这次，跑在
`program_engine_config` 之后约 50 ms、中间还经过 publish 与 knet 轮询。

**下一步**：直接在 `send` 里验证假设 1 —— 例如在提交前后各读一次描述符并对比，
或在 A/B 里把描述符写到**另一块 DMA 缓冲**并用 `dev.res().alloc_coherent` 的
CPU 地址做一次 memcpy 往返；若 CPU 6 上读回的是旧值，即确认多核 TLB/属性不一致。

**下一步（按优先级）**：
1. **顺着 TX 读指针线索做一次干净实验**（见上）：t+0 用 `send` 发第 1 帧、t+5 s
   发第 2 帧，比较两帧的 `reclaimed`。若第 2 帧也不被消费，则指针假说成立，
   修法是"让硬件与软件的指针始终同步"（例如把环基址写入放到**每次**引擎停止后、
   或始终保留一个已 arm 的描述符）。
2. **MSI-X + 中断驱动**（`start.irq-enable`）：本板 INTx 未路由
   （`interrupt line = 0xff`），且内核只有 `MsiBackendIf` 抽象、无 ITS 后端。
3. 取 vendor `r8125` 源码的两条路都没走通：BSP 树是 5.10 稀疏克隆
   （`drivers/net/ethernet/realtek/` 为空），参考板只有二进制 `.ko`。
   还剩两条：板子上 `apt-get source`（需 deb-src + 网络），或
   `objdump -d r8125.ko` 反汇编 `rtl8125_hw_start` 之类函数取写序列。
   在 `ChipCmd` 使能**之前**做的最后一段，我们完全没移植。参考板用的是同一个
   驱动路径，而 U-Boot 的 r8169 不发 EPHY 表也能发——但 U-Boot 的发送紧接着初始化，
   恰好落在"能发"的窗口内，所以它不能作为"不需要 EPHY"的证据。
2. `start.irq-enable` 在当前板上做不了 INTx：`pci header` 显示
   `interrupt line = 0xff`（未路由），中断驱动要先做 **MSI-X**（`pci::msix` 有骨架）。
   优先级低于 1。
3. 若要继续用参考板取证，需要在参考板上编译一个读 0xe4010000 描述符的小模块
   （`/dev/mem` 不可行）。

### 4.1.3 卡点（旧描述，保留）单帧提交不被消费

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
