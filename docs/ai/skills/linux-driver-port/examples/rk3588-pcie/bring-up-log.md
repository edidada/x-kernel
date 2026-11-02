# RK3588 PCIe Bring-up 日志与踩坑记录

本文记录 `drivers/bus/pci/src/dw_host.rs`（rk3588 DesignWare RC）真机 bring-up
的全部过程：已经验证的事实、被证伪的假设、以及每一个坑的**根因与教训**。

写作目的：这些坑每一个都花掉了 1 轮以上的真机周期（约 6 分钟/轮），其中
若干是**看起来完全合理但方向完全错的推断**。后人在同一个 SoC 上继续做
PCIe / USB / 任何高位地址外设时，先读这一页。

---

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

### 下一步（按优先级）


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
