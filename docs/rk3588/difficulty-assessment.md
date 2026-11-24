# 驱动开发难度评估(x-kernel 现状 × RK3588 需求)

> 能力矩阵为 PR 1081 分支(`xiaohui/driver-base-hwrng`,v2 模型 + hwrng
> 样板)实测;难度锚点为主线驱动代码量与新增框架数量。本文件回答:
> 六个方向各自"能不能写、往哪写、被什么挡住"。

## 1. x-kernel 现状能力矩阵

| 能力 | 状态 | 载体 | 说明 |
|---|---|---|---|
| 设备模型(probe/publish/provider/quiesce/release) | ✅ | `drivers/base/model` | DeviceRef/DeviceAccess/Environment,组合根 initcall |
| platform 总线 + DT 匹配 | ✅ | `model/src/bus/platform_bus.rs` | OfMatch/AliasMatch id 表 |
| 资源四轴:MMIO / IRQ(含 MSI) / DMA(coherent+streaming) / 时钟 | ✅ | `drivers/base/os-api` | guard 语义齐全,`dev.res()` 单通道 |
| provider 机制(通用) | ✅ | `model/src/provider.rs` | `ProviderClass` 描述符,任意家族可自建 |
| clock provider(已落地实现) | ✅ | `driver-host` + kclk | 目前唯一实例;gpio/regulator/phy/reset 照抄该模式 |
| 类发布样板 | ✅ | `subsystems/hwrng`(新)/ 旧侧 block/net/display/input/rtc/char/vsock | 新驱动按 hwrng 样板进新模型 |
| 块设备层(gendisk/devnode) | ✅ 旧侧 | block 类 + virtio-blk | eMMC/SD/NVMe 直接复用发布面 |
| 网络栈 + net 类 | ✅ 旧侧 | `net/knet` + net 类 + virtio-net | r8169 的消费面已备 |
| PCI 枚举 / MSI-X | ✅ 旧侧(仅 ECAM) | `drivers/*/pci` | RK3588 无通用 ECAM,**缺 DWC RC 驱动** |
| kirq(GICv3)/ kdma / kclk | ✅ | 内核侧 | 中断/STREAM DMA/时钟后端 |
| USB 核心(URB/hub/枚举) | ❌ | — | 全缺,是 USB 方向真正的大头 |
| MMC 子系统 | ❌ | — | 全缺(命令状态机 + 总线协议 + 卡检测) |
| PHY / GPIO / pinctrl / regulator / reset / pwm | ❌ | — | 全缺,基础四件套 + phy 五个驱动 |
| I2C / SPI 控制器子系统 | ❌ | — | PMIC(RK806=SPI)与 RTC/I2C 的前置 |
| power domain | ❌ | — | pd_usb/pd_gpu/pd_npu 上电前提(U-Boot 预配可顶一阵) |
| IOMMU(rockchip-iommu / smmu-v3) | ❌ | — | VOP 可先直映射物理;NPU 必需 |
| 显示管线(DRM 类比物) | △ 壳 | 旧侧 display 类(静态 DRM 节点投影) | 无 KMS/扫描时序抽象,可裁剪实现 |
| TCPM / Type-C | ❌ | — | 仅 Type-C 口需要;host 口不依赖 |

## 2. 分方向难度评级

评级依据:驱动本体量级 + 需新建的框架数 + 可复用面。参照:x-kernel
`drv/**` 现存约 8.9 K 行(virtio 全家 + serial + rtc)。

| 方向 | 具体目标 | 本体量级(主线) | 前置缺口 | 评级 |
|---|---|---|---|---|
| 时钟 CRU | clk-rk3588 | 120 KB(表为主) | kclk ✅,仅缺驱动 | **低**(机械转写) |
| reset 框架 + CRU reset | rstc | 小 | 框架小件,CRU 复用 | **低** |
| GPIO + pinctrl | gpio-rockchip + pinctrl-rockchip | 21 + 166 KB(表为主) | 需设计 GPIO/pinctrl 契约(provider 照抄 clk) | **中**(框架设计 > 转写量) |
| I2C / SPI / PWM | i2c-rk3x / spi-rockchip / pwm-rockchip | 38/27/10 KB | 需 I2C/SPI 子系统契约 | **中** |
| regulator(RK806) | rk808 MFD + regulator | ~100 KB | SPI ✅ 后;MFD 协议 + fixed reg | **中** |
| PCIe RC | pcie-dw-rockchip + snps-pcie3 + combphy | 28+10+48 KB | 需 PHY 框架;PCI 枚举已有(旧侧) | **中**(收益极大:网×2/NVMe/WiFi) |
| NVMe | nvme/host | 中(可控裁剪) | PCIe + MSI ✅ 概念已有 | **中** |
| 有线网 r8169 | r8169_main 188 KB(可裁剪至初始化+收发) | PCIe;net 类 ✅ | **中**(RTL8125 集成 PHY,免 PHYLIB 大部分) |
| eMMC/SD | sdhci-of-dwcmshc(82)+sdhci(135);dw_mmc(92) | 需**新建 MMC 子系统**;SD 卡检测要 GPIO | **中高** |
| USB2 host | ehci/ohci-platform + usbcore + hub | 15 KB 本体,但 usbcore 中件层需从零设计 | **中高** |
| USB3 host | dwc3(75)+xHCI(172+137 KB) | usbcore + phy 框架 | **高** |
| 显示(最小化) | vop2 裁剪 + dw-hdmi-qp + hdptxphy | 84+35 KB(可裁至固定模式) | vop_mmu 可绕(物理直映);HPD/CEC 要 GPIO | **中高** |
| IOMMU | rockchip-iommu 38 KB(+smmu-v3 远期) | NPU 必需 | **中** |
| NPU | rknpu(BSP,树外)+ 闭源 librknnrt | 内核中件可控;**用户态 ABI 是主要风险** | **高(外部依赖)** |
| GPU panthor | DRM/panthor 全家桶 + smmu-v3 + 固件 | DRM 中件层巨大 | **极高(远期)** |
| WiFi rtw89 | 数万行 + 固件 + mac80211 类比物 | PCIe | **极高(远期,可先 USB 网卡替代)** |

## 3. 建议实施波次

```
Wave A 基础层(解锁一切):CRU clk → reset → GPIO/pinctrl → I2C/SPI/PWM
                          → regulator(RK806)+ power domain(最小)
Wave B 总线与存储:eMMC(sdhci-dwcmshc)→ PCIe RC(+pcie30phy/combphy)
                  → NVMe + r8169(PCIe 上双收益)→ SD(dw_mmc,顺 GPIO)
Wave C USB:phy 框架 + inno-usb2 → usbcore/hub(中件层设计)
            → ehci/ohci(Type-A 可用)→ dwc3/xHCI(USB3)
Wave D 显示:VOP2 最小扫出 + HDMI-QP + hdptxphy(固定模式,不做 KMS 通用层)
Wave E 远期:panthor GPU / rknpu(先做 ABI spike)/ rtw89 WiFi / HDMIRX / 音频
```

- Wave A/B 无 USB 中件层依赖,可完全并行于 Wave C 的框架设计;
- Wave B 的 PCIe RC 是本板**收益密度最高**的单项(网×2 + NVMe + WiFi 四路);
- Wave D 不依赖 GPU:显示输出与算力解耦,小车场景先要的是 HDMI 出图;
- 每波落点建议:契约进 `drivers/contracts` 或新模型 os-api 词汇、
  host 侧进 `driver-host`、家族驱动进 `drv/<family>`、类发布按 hwrng 样板。

## 4. 风险与未决问题

1. **regulator/PHY/GPIO 的 provider 语义**:kclk 的 `ClkProvider` 是唯一
   落地实例;gpio/regulator/phy/reset 四个 provider 家族的键类型(何为
   phandle 语义、消费者查找表)需要一次统一设计,避免四个方言。
2. **usbcore 的中件层形态**:URB 等价物、hub 轮询、设备枚举状态机在
   x-kernel 尚无对应抽象;这是 USB 方向的真实成本,不是 xHCI 寄存器面。
3. **rknpu 用户态**:librknnrt 闭源且依赖 ioctl/mmap/dma-buf 全套 Linux
   语义;建议尽早用一个 1–2 天的 spike 验证最小 ioctl 兼容面,再决定
   是否承诺"AI 推理可用"的时间点。
4. **power domain 最小实现**:U-Boot 已预配大部分域; Wave A 只需
   "查询 + 确保 on"的最小语义即可支撑 B/C/D,完整 PM 后置。
5. **旧侧类与新模型的过渡**:block/net 类在旧侧,eMMC/r8169/NVMe 的
   发布需走 legacy 过渡门(virtio-rng 的 `attach_legacy` 模式)或先行
   迁类;建议每波开始前定一次"迁类 or 过渡门"。
