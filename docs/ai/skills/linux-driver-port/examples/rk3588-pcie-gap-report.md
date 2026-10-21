<!-- 本文件由 `ldp scan --report` 自动生成(草稿)。
人工补全:compatible / DeviceKind / 落点 / 逐条处置 / 可编译性结论 / 集成状态。
模板见 templates/gap-report.md。 -->

# 移植缺口报告(草稿)— ../docs/ai/skills/linux-driver-port/data/references/linux/drivers/pci/controller/dwc/

| 字段 | 值 |
|------|----|
| Linux 源 | `../docs/ai/skills/linux-driver-port/data/references/linux/drivers/pci/controller/dwc/` |
| 目标 compatible | `rockchip,rk3588-pcie`/`snps,dw-pcie`(板载 5 RC;首版目标 pcie2x1l1/l2 —— gen2、1 lane、最简单)|
| 目标 bus | PlatformBus(RC 平台驱动)→ 发布 PciRoot 交 bus/pci 枚举 |
| x-kernel 落点 | `drivers/bus/pci/src/dw_host.rs`(RC 驱动 + DwConfigAccess);bus/** 宿主侧树,边界豁免 |
| 移植结论 | **阻塞→受限路径已定**:前置 = reset(req-reset in-progress)+ phy(req-phy in-progress);MSI 不做(INTx);PERST/供电假定固件已就绪 |

## 依赖的 Linux 子系统清单(scan 自动命中)

- [ ] ❌ `gpio / gpiod` [missing] 命中: devm_gpiod_get, gpiod_set_value
- [ ] ❌ `reset controller` [missing] 命中: reset_control_assert, reset_control_deassert
- [ ] ❌ `regulator` [missing] 命中: devm_regulator_get
- [ ] ❌ `phy` [missing] 命中: phy_init, phy_power_on, devm_phy_get
- [ ] ❌ `completion` [workaround] 命中: complete
- [ ] ✅ `忙等延时(udelay/mdelay/ndelay)` [landed] 命中: udelay, mdelay
- [ ] ❌ `设备子系统无 class(i2c/spi/watchdog/regulator/usb)` [missing] 命中: regulator_get
- [ ] ❌ `MSI / MSI-X(GIC ITS)` [missing] 命中: msi-map
- [ ] ❌ `DesignWare PCIe 主控(RC 模式)` [missing] 命中: dw_pcie_host_init, dw_pcie_write, dw_pcie_read, iatu

## 逐条缺口(scan 自动命中;处置待填)

| 子系统 | 严重度模型 | 现状 | 处置(2026-07 决策) | 需求 issue |
|--------|-----------|------|---------------------|-----------|
| **reset controller** | need_runtime_sequence=blocker | gaps `reset` [missing] | 立项:最小 reset framework + CRU 软复位 provider(rst-rk3588.c 索引表);RC deassert 序列照参考逐条翻译 | req-reset **in-progress** |
| **phy** | independent_phy_ip=blocker | gaps `phy` [missing] | 立项:最小 multi-provider phy 框架 + naneng-combphy / snps-pcie3 两个 RK3588 后端 | req-phy **in-progress** |
| **gpio / gpiod**(reset-gpios=PERST#) | power_reset_pin=works-around | 无 gpio 框架 | 走 works-around 分支:假定 bootloader 已释放 PERST,probe 日志标注假设;出现功能引脚再立项 | req-gpio **rejected** |
| **regulator**(vcc3v3) | fixed_rail=works-around | 无 regulator 框架 | 固定轨,假定固件已使能,日志标注 | req-regulator **rejected** |
| **completion**(LTSSM trace work) | works-around | WaitQueue 替代 | ktask::WaitQueue / kworkng 延后 | — |
| **udelay**(iATU/LTSSM 轮询) | landed | khal::time::busy_wait | 直接用(bus/** 宿主侧树) | — |
| **设备 class**(scan 误报) | — | n/a | RC 是 bus infra,不需新 class | req-device-class **rejected** |
| **MSI / MSI-X(GIC ITS)** | conditional | gaps `msi` [missing] | v1 走 INTx(interrupt-map 已由总线解析进 IrqSpec);ITS 待 msi 立项 | (msi 保持 open) |
| **DesignWare RC 本体** | conditional | gaps `pci-dwc-host` [missing] | 立项:dw_host(DBI+iATU 配置访问+出站窗口+LTSSM),挂进 drivers/bus/pci;枚举复用 PciRoot | req-pci-dwc-host **in-progress** |

## 阻塞项汇总(决策门,生成代码之前必须过)

- ⚠ `gpio`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `reset`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `regulator`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `phy`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `device-class`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `msi`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。
- ⚠ `pci-dwc-host`: 严重度模型含 conditional/blocker,尚无已决策需求 issue(无)。
  - 下一步:`ldp requirements gen --from-scan <路径>` 生成 issue,人工 `confirm`
    后 `ldp scan <路径> --gate` 通过,才能进入代码生成。

## 可编译性结论

- 待 Phase 5(P3 代码生成后):`make build`(rk3588_defconfig)+ `make clippy`。

## 移植顺序(解 blocker 的拓扑)

1. req-reset:reset framework + CRU 软复位 provider(同时服务 USB 切片)。
2. req-phy:phy provider 框架 + combphy(pcie2 模式)——首版目标 pcie2x1l1/l2 只需要 combphy。
3. dw_host:DBI/iATU/LTSSM + INTx + PciRoot 集成。
4. pcie3(snps-pcie3-phy)在后,pcie2 跑通后增量。

## 集成状态

- [x] P0-P2 完成(本报告 + portrait + 需求决策);P3 待下一编码段(先 reset/phy 两个前置框架)
> 在草稿上继续补全(处置、编号参考序列对照表、集成记录),完成后即正式缺口报告。
