# RK3588 USB 移植画像(P0/P1)— ehci/ohci + usb2phy → x-kernel

## P0 采集 — 移植任务卡

| 字段 | 值 |
|------|----|
| Linux 源 | `drivers/usb/host/ehci-hcd.c`(+ehci-hub/ehci-sched)、`ohci-hcd.c`;平台胶水即 `generic-ehci`/`generic-ohci`(HCD core 的 platform 端);RK3588 无 vendor 寄存器补丁(兼容 generic)|
| compatible | `rockchip,rk3588-ehci`+`generic-ehci` / `generic-ohci`(板载 2 对:fc800000/fc840000、fc880000/fc8c0000;companion 关系已由 DT `companion` 描述)|
| 类/栈 | USB 主机栈(usb core + HCD)→ 首版 host-only:req-usb-stack **in-progress** |
| x-kernel 落点 | 新 `drivers/subsystems/usb`(usb core 最小面:hub 枚举/地址/描述符)+ `drivers/drv/usb`(ehci/ohci HCD + rk3588 usb2phy)|
| DT(真机) | 每对:reg 0x40000 窗口、1 IRQ、clocks(usbhost/arbiter/utmi/alk_usb)、phys=<&usb2phy u2>、power-domains(PD,缺 power-domain 框架 → 缺口报告)|

## P1 分析 — 驱动画像

### 1. probe 流程(ehci 平台 probe)

1. ioremap reg 窗口;`ehci_setup`:caps/operational 寄存器解析(HCSparams/HCCparams)→ `ehci_reset` → `ehci_init`(_periodic/_async list)→ `ehci_run`(`USBCMD` run/async+periodic enable,`CONFIGFLAG CF=1`)。
2. `devm_usb_get_phy` + `usb_phy_init`(rk3588 usb2phy:GRF 寄存器组,`phy-rockchip-inno-usb2.c` — utmi pin/tx/rx tuning、`soft-con-reset`,经 `usbctrl-grf`/`usb2phy-grf` syscon;req-phy 共用其 provider 框架)。
3. 时钟:`usbhost/arbiter/utmi/alk_usb` ×4(CRU 已落地,按名取)。
4. IRQ:ehci 中断(qh 链推进)+ root hub 注册(usb core)。
5. companion 关系(ETCI/ehci-ohci 交接):Linux `companion` phandle + `ehci->has_tt`?;首版姿态:OHCI/ehci 各自全速/低速设备经 OHCI,通用语义后置(缺口报告记录)。

### 2. 依赖子系统(scan 命中 → 决策)

| Linux API | 用途 | 决策(需求 issue) |
|-----------|------|------------------|
| `dma_map_single` 等 streaming | qtd 缓冲 | req-dma-streaming **rejected**:首版全 coherent(qh/qtd/缓冲都用 alloc_coherent) |
| `usb_hcd` 栈(usb core/hub) | 设备模型 | req-usb-stack **in-progress**(本切片本体) |
| `phy_init`/usb2phy | UTMI PHY | req-phy **in-progress**(与 PCIe 共用 provider 框架;usb2phy 是第三后端) |
| power-domains(`power-domains=<PD 31>`) | PD 开关 | **缺口新增**(power-domain framework,随 pd 立项;首版假定固件已使能,日志标注) |
| `timer_setup/mod_timer`(ehci watchdog) | 轮询计时 | ✅ ktimer-core 已落地 |
| `vmalloc`(debug) | 仅 dbg | 不移植(可选) |

### 3. 寄存器序列(铁律 1,实现时逐条对照参考)

真相源:linux-7.1.8 `drivers/usb/host/ehci.h` + `ehci-hcd.c`(已 vendor)。关键序列:`ehci_reset`(CMD_RESET 轮询)、`ehci_run`(USBCMD_RS/IAA/ASE/PSE、CTRLDSSEGMENT、INTR_ENABLE)、periodic/async list 构建(qh→qtd,`QH_HLNK` 等 bit 进 bitfield-defs,scope=port rk3588-usb)。

### 4. 首版验收形态(无真机时的编译级)

- `subsystems/usb` 最小 usb core:device 地址池、标准描述符解析、hub 驱动的端口事件轮询(无 MSI/中断风暴担忧,INTx 足够)。
- EHCI 先于 OHCI(EHCI 能 cover 高速;全速/低速经 ehci 的 TT?——RK3588 ehci 无内嵌 TT,全/低速走 companion OHCI → OHCI 是同一批次的必要件)。
- usb2phy 作为 phy provider 后端(cut:只 init/power_on;调参序列照 phy-rockchip-inno-usb2.c 的 RK3588 段)。
