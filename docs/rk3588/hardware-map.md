# RK3588 / Orange Pi 5 Plus 硬件清点

> 来源:主线 torvalds/linux master DT(`rk3588-base.dtsi` 3634 行、
> `rk3588-extra.dtsi` 695 行、板级 `rk3588-orangepi-5{,-plus}`)。
> "主线驱动"列给出 Linux 驱动文件与实测代码量(kernel.org),量级仅供
> 难度锚定。"OPi5+"列为本板实际接线(是否启用/接到哪个外部器件)。

## 0. 全局基础(所有方向共用)

| DT 节点@地址 | compatible | 主线驱动 / 量 | OPi5+ 接线 | 备注 |
|---|---|---|---|---|
| cru@fd7c0000 | `rockchip,rk3588-cru` | `clk-rk3588.c` / 120 KB | U-Boot 预配,内核侧运行时 | 时钟 + **复位控制器**一体 |
| pinctrl(fd8*. GRF) | `rockchip,rk3588-pinctrl` | `pinctrl-rockchip.c` / 166 KB | 板级 pinctrl-0 全量引用 | GPIO0–4 bank + pinmux/pinconf |
| gpio0–4 | `rockchip,gpio-bank` | `gpio-rockchip.c` / 21 KB | SD 卡检测、Type-C IRQ、PCIe reset/perst、HDMI HPD/CEC、LED、PMIC IRQ | 无 GPIO 框架则上列全不可用 |
| spi2 上的 rk806 | `rockchip,rk806` | `rk808.c`(MFD)+ `rk808-regulator.c` / 74 KB | 板上 PMIC,**SPI2 cs0**,IRQ=GPIO0_7 | 全板核心电源域来源(vdd_gpu_s0/vdd_npu_s0/…) |
| power(pd_*) | `rockchip,rk3588-power-domain`(pm_domain) | `drivers/pmdomain/rockchip/` / ~40 KB | pd_gpu/pd_npu/pd_usb 等 | VOP/NPU/GPU 上电前提 |
| 各 GRF syscon | `rockchip,rk3588-*-grf` | syscon,无独立驱动 | usb2phy-grf/pipe-phy-grf/usbdpphy-grf/hdptxphy-grf/vop-grf/… | PHY/显示驱动的寄存器窗 |
| saradc | `rockchip,saradc` | `rockchip_saradc.c` | 已启用 | 低优先 |
| thermal + fan | `rockchip,rk3588-thermal` + pwm2/pwm3 | thermal + `pwm-rockchip.c` / 10 KB | 风扇温控 | 非阻塞,后置 |
| package_thermal | 同上 | | 已启用(1000ms 轮询) | |

## 1. USB Host 及典型外设

SoC 侧构成(均为板级已启用,除注明):

| DT 节点@地址 | compatible | 主线驱动 / 量 | OPi5+ 接线 |
|---|---|---|---|
| usb@fc000000 (`usb_host0_xhci`) | `rockchip,rk3588-dwc3`,`snps,dwc3` | `dwc3/core.c` / 75 KB | **Type-C 口**(DRD),SS=usbdp_phy0,HS=u2phy0_otg |
| usb@fc400000 (`usb_host1_xhci`,extra die) | 同上 | 同上 | **USB3 Type-A**,host 模式,SS=usbdp_phy1 |
| usb@fcd00000 (`usb_host2_xhci`) | 同上 | 同上 | **本板未启用**(lane 让给 PCIe 网口) |
| usb@fc800000/fc880000 (ehci) | `rockchip,rk3588-ehci` | `ehci-platform.c` / 15 KB | 2× USB2 Type-A |
| usb@fc840000/fc8c0000 (ohci) | `rockchip,rk3588-ohci` | ohci-platform | 同上 |
| usb2phy0/2/3(+otg/host 子节点) | `rockchip,rk3588-usb2phy` | `phy-rockchip-inno-usb2.c` / 71 KB | 全启用;u2phy2/3_host 供 EHCI/OHCI |
| usbdp_phy0/1 | `rockchip,rk3588-usbdp-phy` | `phy-rockchip-usbdp.c` / 43 KB | Type-C(USB3/DP 复用)与 USB3 Type-A |
| combphy2_psu | `rockchip,rk3588-naneng-combphy` | `phy-rockchip-naneng-combphy.c` / 48 KB | 本板用作 PCIe(右侧网口),不用 USB3 |
| i2c6 上的 fusb302@22 | `fcs,fusb302` | `tcpm/fusb302.c` / 50 KB | Type-C PD/角色切换(TCPM 框架) |
| vbus5v0_typec / vcc5v0_usb20 | `regulator-fixed`+GPIO | regulator-fixed | VBUS GPIO 使能 |

外设类(典型 USB 设备):`usb-storage`/uas、`usbhid`、cdc-ncm/rndis、
cp210x/ch341 等串口 — 每个都是 usbcore 之上的 class 驱动。

**依赖链**:phy 框架 → inno-usb2/usbdp → USB 核心(URB/hub/设备枚举)→
ehci/ohci(易)→ dwc3+xHCI(难)→ 外设 class 驱动。Type-C(TCPM)可后置:
host 专用口不需要。

## 2. 有线/无线网络

| DT 节点@地址 | compatible | 主线驱动 / 量 | OPi5+ 接线 |
|---|---|---|---|
| gmac0/gmac1@fe1c0000 | `rockchip,rk3588-gmac`,`snps,dwmac-4.20a` | `dwmac-rk.c` / 47 KB | **本板未接线**(OPi5+ 不用 GMAC) |
| pcie2x1l2@fe190000(phy0=combphy0_ps) | `rockchip,rk3588-pcie` | `pcie-dw-rockchip.c` / 28 KB | **左网口:RTL8125BG**(r8169) |
| pcie2x1l1@fe180000(phy2=combphy2_psu) | 同上 | 同上 | **右网口:RTL8125BG** |
| pcie2x1l0@fe170000(phy1=combphy1_ps) | 同上 | 同上 | **M.2 E-key:WiFi(如 RTL8852BE)** |
| pcie3x4@fe150000 + pcie30phy | `rockchip,rk3588-pcie`,`rockchip,rk3588-pcie3-phy` | pcie-dw + `phy-rockchip-snps-pcie3.c` / 10 KB | **M.2 M-key:NVMe** |
| (PCI 函数级)RTL8125 | `10ec:8125` | `r8169_main.c` / 188 KB | 2.5 GbE ×2 |
| (PCI 函数级)RTL8852BE | `10ec:b852` | `rtw89/`(数万行) | WiFi6,远期 |

**结论**:本板有线网 = PCIe RC + r8169 + 以太网 PHY(板上 RTL8221B
集成在 RTL8125 内,走 PHYLIB 词汇)。GMAC/dwmac-rk 对本板无用,但对
其他 RK3588 板卡是主线形态,可作为第二优先。无线 = PCIe + rtw89,
建议远期或先以 USB 网卡顶替。

## 3. GPU

| DT 节点@地址 | compatible | 主线驱动 / 量 | OPi5+ 接线 |
|---|---|---|---|
| gpu@fb000000 | `rockchip,rk3588-mali`,`arm,mali-valhall-csf` | `drm/panthor/`(全家桶) | 已启用;pd_gpu 供电 vdd_gpu_s0 |
| mmu600(iommu@fc900000 等) | `arm,smmu-v3` | `arm-smmu-v3/` | GPU 侧 IOMMU,panthor 依赖 |

**结论**:panthor = DRM 设备模型 + GPU 调度器 + 固件(CSF)+ 堆管理 +
smmu-v3,是全清单最重的单项;显示输出不依赖它(见 §5),推理算力靠
NPU(见 §4)。建议列为远期,x-kernel 先满足"能挂起/能跳过"即可。

## 4. NPU / AI 推理

| DT 节点@地址 | compatible | 主线驱动 | OPi5+ 接线 |
|---|---|---|---|
| npu@fdab0000/fdac0000/fdad0000(`rknn_core_0/1/2`) | `rockchip,rk3588-rknn-core` | **主线无驱动**(BSP `drivers/rknpu`,树外) | 已启用;vdd_npu_s0(RK806)+ pd_npu |
| rknn_mmu_0/1/2 | `rockchip,rk3588-iommu`,`rockchip,rk3568-iommu` | `rockchip-iommu.c` / 38 KB | 已启用 |

**结论**:硬件描述全在主线 DT;内核侧缺口 = rknpu 类驱动(MMIO+DMA+
IOMMU+ioctl 面)+ rockchip-iommu;真正的难点是**闭源用户态运行时
librknnrt 的 ABI**(ioctl/mmap/dma-buf/依赖的 LTP 风格接口)。建议先做
ABI 调研 spike,再决定 ioctl 兼容层策略。

## 5. 存储 / 显示

存储:

| DT 节点@地址 | compatible | 主线驱动 / 量 | OPi5+ 接线 |
|---|---|---|---|
| sdhci: mmc@fe2e0000 | `rockchip,rk3588-dwcmshc` | `sdhci-of-dwcmshc.c` / 82 KB(+`sdhci.c` / 135 KB) | eMMC 8bit HS400ES@200MHz,vmmc/vqmmc=RK806 |
| sdmmc: mmc@fe2c0000 | `rockchip,rk3588-dw-mshc` | `dw_mmc.c` / 92 KB | SD 卡,cd=GPIO0_A4,SDR104(vqmmc 1.8V 切换) |
| sfc: spi@fe2b0000 | `rockchip,sfc` | `rockchip-sfc` | SPI NOR(bootloader 用) |
| pcie3x4 + NVMe 盘 | — | `nvme/host/` | M.2 M-key,见 §2 |

显示:

| DT 节点@地址 | compatible | 主线驱动 / 量 | OPi5+ 接线 |
|---|---|---|---|
| vop@fdd90000(+4 video port) | `rockchip,rk3588-vop` | `rockchip_drm_vop2.c` / 84 KB | vp0→hdmi0,vp1→hdmi1 |
| vop_mmu@fdd97e00 | `rockchip,rk3588-iommu` | `rockchip-iommu.c` / 38 KB | VOP IOMMU |
| hdmi0@fde80000 / hdmi1 | `rockchip,rk3588-dw-hdmi-qp` | `dw-hdmi-qp.c` / 35 KB | 2× HDMI 2.1 TX(CEC/HPD=GPIO+pinctrl) |
| hdptxphy0/1 | `rockchip,rk3588-hdptxphy` | `phy-rockchip-hdptx` | HDMI 物理层(QP 版) |
| dp0@fde50000 | `rockchip,rk3588-dp` | `analogix/cdn-dp` 类 | 未接线(可忽略) |
| hdmirx@fdee0000 | `rockchip,rk3588-hdmirx` | `rockchip-hdmirx` | HDMI 采集,可选,后置 |

**结论**:x-kernel 侧 block 类已存在(virtio-blk 样板),存储缺的是
**MMC 子系统本身**(命令状态机 + 总线协议)与两个 host 驱动;NVMe 依赖
PCIe,量级中等。显示可走"最小化裁剪":固定 EDID/模式的 VOP2 扫出 +
HDMI-QP 初始化(HPD/CEC 简化),不引入 DRM 通用层;GPU 加速与显示解耦。

## 6. 其他板级外设(低优先)

| 器件 | 总线/节点 | 主线驱动 | 价值 |
|---|---|---|---|
| RTC hym8563 | i2c(I2C 上的 0x51) | `rtc-hym8563.c` / 14 KB | x-kernel 已有 rtc 类,易迁移,I2C 之后顺手的第一个 I2C 客户端 |
| IR 接收头 | gpio4_B3 | gpio-ir-receiver | input 类已有(旧侧) |
| 音频 es8388 + i2s×4 | i2s/sound | ASoC | 远期 |
| 看门狗 wdt | `snps,dw-wdt` | dw_wdt | 稳定性,后置 |
| uart2(控制台)/uart9(BT) | `snps,dw-apb-uart` | 已有 serial-core | uart9 供 BT,后置 |
