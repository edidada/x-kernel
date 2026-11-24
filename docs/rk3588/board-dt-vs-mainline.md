# 板上实际 DT(BSP 形态)与主线 DT 的对比

> 对象:`xiaohui/driver-test` 分支根目录 `orangepi-5-plus-m.dts`
> (13118 行,反编译扁平形态,U-Boot/板卡实际交付的 DT)。
> 参照:主线 torvalds `rk3588-orangepi-5(-plus)` 包含链。
> 结论:**硬件事实完全同源,形态不同**——这是一份 Rockchip BSP(6.1 风格)
> 设备树,不是主线形态;对本仓库的规划没有推翻项,只有两个补充决策点。

## 1. 一致的关键硬件事实(逐项核对过 status)

| 事实 | 板上 DT | 主线 | 一致? |
|---|---|---|---|
| GMAC(fe1b/fe1c)未接线 | 均为 `disabled` | 板级不启用 | ✅ |
| PCIe 启用集 = 3x4(fe150000)+ 2x1l0/1/2(fe17/18/19) | 3x4/l0/l1/l2 `okay`,3x2 disabled | 同集合(l0=E-key,l1/l2=网口) | ✅ |
| usb@fcd00000(host2 xHCI)未用,lane 让给 PCIe | `usbhost3_0` disabled | 板级不启用 | ✅ |
| USB 拓扑:fc000000(OTG)+ fc400000(host)+ 2×EHCI/OHCI | usbdrd3_0(otg)/usbdrd3_1(host)okay | 同 | ✅ |
| PMIC = RK806 @ SPI2 cs0 | `rk806single@0` okay(spi@feb20000) | `rockchip,rk806` on spi2 | ✅ |
| combphy×3 + pcie3-phy | 全 okay | OPi5+ 启 0/1/2 | ✅ |
| fusb302@0x22 + hym8563@0x51 在 i2c6(fec80000) | 在 | 在 | ✅ |
| SD(fe2c0000)/eMMC(fe2e0000) | 均 okay | 均启用 | ✅ |

## 2. 形态差异(转写时按哪份建模的问题)

| 项 | 板上 DT(BSP) | 主线 | 影响 |
|---|---|---|---|
| 板命名 | `rockchip,rk3588-orangepi-5-plus` | `xunlong,orangepi-5-plus` | id 表匹配串不同 |
| NPU | **单节点** `rockchip,rk3588-rknpu`(三核一体,含 opp 表) | `rknn_core_0/1/2` + 各自 IOMMU | 建模决策点(见下) |
| GPU 绑定 | `arm,mali-bifrost`(kbase 生态) | `arm,mali-valhall-csf`(panthor) | 用户态生态决策点(见下) |
| USB 层级 | `usbdrd3_N` wrapper + `syscon/usb2-phy@x/{otg,host}-port` | 平铺 `usb@…` + `usb2phy-grf` | 解析时多一层嵌套,资源(reg/interrupts)位置不同 |
| eMMC compatible | `rockchip,rk3588-dwcmshc | rockchip,dwcmshc-sdhci` | `rockchip,rk3588-dwcmshc` | 多一条 BSP 变体名 |
| 显示 | **HDMI0/1 均 `disabled`**,DSI0+panel 结构在但 disabled,VOP okay | HDMI0/1 enabled | 板上当前未配显示输出 |
| 触摸屏 | **gt9271@0x14(i2c7)`okay`** | 无 | input 方向的现成目标 |
| console uart2 | DT 内 `disabled`(chosen stdout-path 仍指向它) | enabled | x-kernel 走 `BOOT_CONSOLE_ADDR`,无影响 |
| BSP 特有节点 | `clock-gate-link`、`hdmiphy@…`(时钟节点)、`display-subsystem/routes`、otp、firmware 等 | 无或不同 | 解析器需容忍未知节点 |

## 3. 对规划的影响

1. **无推翻项**:PCIe 优先、PMIC=SPI2、GMAC 不做、USB 拓扑——原评估全部成立;
   这份板上 DT 就是 `firmware/of`(rs-fdtree)实际要吃的那份事实源。
2. **NPU 建模决策**(建议):按主线 `rknn_core_*` 三核 + IOMMU 拆分建模
   (硬件描述更干净、IOMMU 边界显式),寄存器窗两边一致,不损失信息;
   BSP 单节点形式仅作交叉核对。
3. **GPU 生态决策**:若目标是跑 Rockchip 的闭源用户态(bifrost/kbase 生态),
   绑定与 ABI 跟着 BSP 走;若跟主线 panthor/CSF,则按主线建模。建议在
   GPU 立项时用"用户态要跑什么"反推,而不是先改 DT。
4. **显示方向**:当前板上 HDMI 未启用——上板验证显示前需先打开 HDMI0/1
   节点(或改走 DSI LCD 路线);触摸屏已启用,可作为 input 类上板的
   第一个真实设备(goodix, I2C)。
