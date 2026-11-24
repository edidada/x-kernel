# RK3588 硬件清点与驱动开发难度评估

> 分支:`xiaohui/rk3588-hw-survey`,基线:PR 1081(`xiaohui/driver-base-hwrng`)。
> 目的:在 v2 驱动模型落地期间,先摸清 RK3588(目标板 Orange Pi 5 Plus)六大
> 需求方向(USB / 网络 / GPU / NPU / 存储与显示 / 整机)各自的硬件构成、
> Linux 主线驱动形态、对 x-kernel 现有能力与缺失框架的依赖,从而给出
> 驱动开发的难度排序与实施波次建议。

## 文档索引

| 文件 | 内容 |
|---|---|
| [`hardware-map.md`](hardware-map.md) | 硬件清点:SoC/板级 DT 节点、compatible、主线驱动与代码量、OPi5+ 实际接线 |
| [`difficulty-assessment.md`](difficulty-assessment.md) | x-kernel 能力矩阵(实测 PR 1081 分支)、每方向难度评级、建议实施波次 |
| [`board-dt-vs-mainline.md`](board-dt-vs-mainline.md) | 板上实际 DT(`xiaohui/driver-test` 的 BSP 形态反编译件)与主线的逐项对比 |
| [`sd-line-status.md`](sd-line-status.md) | SD/MMC 线实测现状(`xiaohui/driver-test`:MMC 栈已成形,阻塞在框架时钟依赖) |
| [`work-plan.md`](work-plan.md) | 下一步工作计划(T0 SD 闭环 → eMMC → USB/PCIe 并行 → 显示) |

## 数据来源

- 设备树:主线 torvalds/linux master
  `arch/arm64/boot/dts/rockchip/`,包含链
  `rk3588-orangepi-5-plus.dts → rk3588-orangepi-5.dtsi → rk3588.dtsi
  → rk3588-extra.dtsi / rk3588-base.dtsi(+ pinctrl 三个 dtsi)`。
- 驱动代码量:主线 torvalds/linux master 对应文件字节数(kernel.org
  content-length 实测),仅作量级锚点,不代表需移植的全部。
- NPU 运行时:`rknpu` 驱动不在主线(DT 节点在),用户态 `librknnrt`
  为闭源 blob,需另行研究 ABI 兼容路径。

## 关键结论(速览)

1. **PCIe 是 OPi5+ 上三个方向的共同瓶颈**:两个有线网口是挂在
   `pcie2x1l1/l2` 上的 RTL8125(本板不使用 RK GMAC),WiFi(M.2 E-key)与
   NVMe(M-key)也在 PCIe。先打通 `pcie-dw-rockchip` + combphy,收益最大。
2. **USB3/PCIe/SATA 共享 lane(combnaneng phy)**,板级 DT 决定复用方式;
   OPi5+ 未启用 `usb_host2_xhci`,其 lane 让给了右侧网口。
3. **PMIC RK806 挂在 SPI2**(`rockchip,rk806`),不是 I2C:regulator
   方向的前置是 SPI 子系统 + MFD 协议,而非 I2C。
4. **主线 DT 已含 NPU 节点**(`rknn_core_0/1/2` + 各自 IOMMU),硬件描述
   可直接取用;缺口在运行时驱动与闭源用户态栈。
5. x-kernel 侧(PR 1081 分支):platform 总线 + DT 匹配、MMIO/IRQ(+MSI)/
   DMA/时钟四轴资源、通用 provider 机制、hwrng 样板均已就绪;USB core、
   MMC、PHY、GPIO/pinctrl、regulator、reset、I2C/SPI、PWM、power domain、
   IOMMU、显示管线、TCPM 全部缺失。
