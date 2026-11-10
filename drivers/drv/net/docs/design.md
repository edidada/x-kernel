# drv-net — 设计文档

## 定位

driver-base 模型下的**网络接口（NIC）驱动**聚合 crate。一个子系统一个 crate，
一个驱动一个模块（与 `drv/virtio`、`drv/serial`、`drv/rtc` 同构）。

当前只有一个驱动：`rtl8125`，认领 PCI id `10ec:8125`，是 RK3588 板上
（Orange Pi 5 Plus）那颗 Realtek 2.5GbE。

## 范围

| 文件 | 内容 |
|---|---|
| `src/lib.rs` | crate 门面（`#[cfg(feature = "rk3588-rtl8125")] pub mod rtl8125;`） |
| `src/rtl8125.rs` | `Rtl8125Driver` + `Rtl8125Data` + `IDS` + `register()` |

**当前状态**：绑定 → 识别型号 → 读站地址 → 复位 → 配置 → 环 → 发布到 `net`
类。数据通路**还没打通**：MAC 不取描述符（详见
`docs/ai/skills/linux-driver-port/examples/rk3588-pcie/bring-up-log.md` §4.1
里已排除的十条假设）。

## 为什么分阶段

一个"已经出现在系统里但发不出包"的 NIC 比一个不存在的 NIC 更糟。当前实现
先把 **环、描述符所有权、类契约** 全部落地并在真机上跑起来（`send` 被调用、
描述符被正确写入、knet 建出 eth0 并 UP），只差 MAC 主动来取描述符这一步；
这个状态下用户态能看到网卡但 ping 不通，`docs/design.md` 与 bring-up log 都
明确记录了这一点，不假装能用。

## 数据通路结构

```text
knet ──NetDevice──► Rtl8125Nic { bar0, rings(CoherentMem), pool, Mutex<RingState> }
                      send:  拷贝 frame → TX slot → 写 opts1(OWN|FIRST|LAST|len) → TxPoll_8125
                      recv:  扫 RX 环（OWN 已清）→ 拷贝 slot → NetBuf → 立刻 re-arm 描述符
                      recycle_tx: 回收 OWN 已清的 TX 描述符
```

- **双向都是拷贝**：描述符所有权保持"单写者"不变量；代价是每包一次 memcpy，
  相对总线事务可以忽略。
- **RX 在 `recv` 里立刻 re-arm**：栈侧回收只把 `NetBuf` 还给 pool，不与 DMA
  生命周期耦合。
- 顺序保证用 `fence(Release)`（交所有权前）/`fence(Acquire)`（看 OWN 之后）。

## 架构

```text
pci::discover::discover（level4）
  └─ 注册 PciBus 设备（PciBusData: segment/bus/dev/fn + vendor/device/class）
        │  领域内匹配 IDS
        ▼
Rtl8125Driver::probe(dev)
   ├─ dev.res().mmio(0) → BAR0（64 KiB 寄存器窗口）
   ├─ dev.res().iomap_region(…) → IoMap（Device 属性，nGnRnE）
   ├─ TxConfig(0x40) → xid = (TxConfig >> 20) & 0xfcf → CHIP_INFOS 查表
   ├─ MAC0/MAC4(0x00/0x04) → [u8; 6] 站地址
   └─ Rtl8125Data { _bar0, mac, chip, xid }   // IoMap 跟 Data 走，绑定期存活
```

### 型号识别的两个必须

1. **型号来自芯片，不是 PCI id**：`10ec:8125` 覆盖 RTL8125A/B/BP/CP/D，
   后续每一步（复位序列、描述符环、PHY 访问、中断掩码）都按 MAC version
   分支。判据与 `r8169` 一致（`rtl_chip_infos[]` 的 `mask/val`）。
2. **`~0` 守卫必须在算术之前**：BAR 解码不到时读回全 1，`(u32::MAX >> 20) & 0xfcf`
   是一个看着很正常的 id。先判全 1，再算 xid。
   （这条来自 PCIe bring-up 的教训：写进寄存器再读回什么也证明不了，
   见 `docs/ai/skills/linux-driver-port/examples/rk3588-pcie/bring-up-log.md` §3.4.4。）

### 站地址的守卫

`MAC0 == 0 || MAC0 == u32::MAX` 视为设备未就绪并让 probe 失败：站地址是
消费者唯一会拿去用的东西，一个不可能的地址意味着芯片还没准备好，比"绑定
成功但地址是垃圾"更值得报错。

## 资源与生命周期

- BAR0 的 `IoMap` 由 `Rtl8125Data` 持有，随 `Device` 卸载释放。
- 只取 BAR0。BAR2/BAR3 是 MSI-X table，等中断切片再取。
- 没有 reset、没有写操作：当前 probe 是**纯读**，因此不改变设备状态，
  重复 probe 安全。

## 依赖边界

- 依赖 `driver_base`（设备模型、`IoMap`）与 `pci`（`PciBus` / `PciMatch`）。
  `pci` 是 host-side 树但**按角色**：`pci_bus_type` 归 PCI 子系统（Linux 同样
  把 `pci_bus_type` 放在 `drivers/pci`），`drv/virtio` 也是这样取 `PciBus`。
- 不依赖 `knet`：类发布是下一步的事，驱动不应直接摸协议栈。

## 下一步（按顺序）

1. **复位 + PHY/链路就绪**：`ChipCmd |= CmdReset` → 轮询复位完成 → 解锁
   `Cfg9346` 配置 → 使能 RX/TX。参考 `r8169_main.c` 的
   `rtl8168_hw_reset` / `rtl_hw_initialize` 分支（8125B 走
   `RTL_GIGA_MAC_VER_63` 路径）。
2. **描述符环**：TX/RX 各 256 个描述符（`RX_DESC_COUNT` 的取值随环落地），
   全 coherent DMA（`kdma`），先轮询不中断。
3. **发布 `net` 类**：实现 `subsys_net::NetDevice`，`dev.publish(&NET, …)`；
   届时才有 `can_tx`/`can_rx`/`send`/`recv`。
4. **中断 / MSI**：`kirq` + devres IRQ 守卫，替换轮询。
5. **多 host**：`PUBLISHED_CAM` 目前是单槽，板上第二块 8125（另一控制器）
   还不会进域。