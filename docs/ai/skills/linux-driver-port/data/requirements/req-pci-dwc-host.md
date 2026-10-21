<!-- 本文件由 `ldp requirements gen` 生成。手动确认/补充后即可作为正式需求 issue。-->

# 实现 DesignWare PCIe 主控(RC 模式)(dw_pcie_host_init)

| 字段 | 值 |
|------|----|
| ID | `req-pci-dwc-host` |
| 关联缺口 | `pci-dwc-host` (gaps.json) |
| 分类 | `driver-missing` |
| 状态 | `in-progress` |
| 来源 | `scan:../docs/ai/skills/linux-driver-port/data/references/linux/drivers/pci/controller/dwc/pcie-dw-rockchip.c` |
| 触发符号 | `dw_pcie_host_init` |
| 发现备注 | 命中符号: dw_pcie_host_init, dw_pcie_write, dw_pcie_read |

## 背景

**决策(2026-07,in-progress)**:立项移植 pcie-dw-rockchip + dw 核心(dbi/iatu/link)到 drivers/bus/pci(v2 非宿主侧,现只有 ECAM)。首版范围:DBI+APB 寄存器访问、iATU 出站窗口(ranges 解析)、CfgRd/CfgWr 经 iATU、LTSSM 状态读取、INTx 经 legacy-interrupt-controller(interrupt-map 已解析进 IrqSpec)。MSI(ITS)明确不在首版(gaps msi 另立)。

drivers/bus/pci 只覆盖 ECAM(MmioCam,virtio QEMU 路径)。DesignWare RC 无 ECAM:配置访问走 DBI+iATU CfgRd/CfgWr,RK3588 还要 pipe-grf/PHY/PERST#。需要 host 桥 driver(bus/pci 或新 crate)。

Linux 侧相关符号:`dw_pcie_host_init`, `dw_pcie_write`, `dw_pcie_read`, `pcie-dw-rockchip`, `iatu`。

## 建议实现路径

单纯驱动缺失(基础设施都在):
1. 底层 class 契约 + publish + 所需子系统都已有,只是这个具体驱动没移植。
2. 直接按 `templates/driver-skeleton.md` 走 Phase 0-6,不动基础设施。
3. 不需要新建框架;本 issue 的价值在于记录「哪些驱动待移植」的 backlog。

## 验收标准

- [ ] `ldp check-data && ldp check-bitfields && ldp golden` 全过
- [ ] gaps.json 对应条目 `status` 改 `landed`
- [ ] 驱动按 skeleton 实现,build + clippy 过
- [ ] P6 带机验证

---

> **人工确认流程**:审查本文件 → `ldp requirements confirm req-pci-dwc-host` 标 `confirmed` → 进入实现 → 实现后把对应 gaps.json 条目的 `status` 改 `landed` 并跑 `ldp check-data && gen-doc`。
> 分类若不对,编辑 `data/gaps.json` 对应 subsystem 的 `infra_type` 字段后重新 `ldp requirements gen`。
