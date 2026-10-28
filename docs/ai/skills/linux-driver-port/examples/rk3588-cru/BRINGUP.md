# RK3588 CRU — 真机 bring-up 手册（Orange Pi 5 Plus）

前置：`KFEAT_DRIVER_CLK_RK3588_CRU=y` 已在 `platforms/kplat-aarch64/rk3588_defconfig`。

## 1. 构建

```bash
cp platforms/kplat-aarch64/rk3588_defconfig .config
make defconfig
make build
# 产物: target/xkmake/kplat-aarch64/release/kernel.bin （raw ARM64 Image）
```

## 2. 传输与启动

U-Boot 已在 SD/eMMC/tftp 环境中就绪（RK3588 SPI/SD 启动）。任选：

**TFTP**：
```tftp
tftpboot ${loadaddr} xkernel/kernel.bin
booti ${loadaddr} - ${fdtcontroladdr}
```
`${fdtcontroladdr}` 是 U-Boot 自己的 control DTB —— 正是 orangepi-5-plus.dts
编译产物，时钟 phandle/编号与移植约定一致，无需本地 dtc。

**SD 卡**：`dd if=kernel.bin of=<分区>/boot.img`，U-Boot `load mmc 1:1 ...` 后同上。

串口：调试口 1500000 8N1（与 BOOT_CONSOLE_BAUDRATE=0x16e360 一致）。

## 3. 验收清单（P6 smoke gate）

| # | 日志证据 | 含义 |
|---|----------|------|
| 1 | `clk-rk3588: provider 0x2 ready; gpll=1188000000Hz cpll=786432000Hz npll=850000000Hz ppll=1100000000Hz pclk_top_root=100000000Hz sclk_uart2=<N>Hz` | CRU probe 成功，全树注册，关键钟速率与 BSP/DT assigned 期望一致 |
| 2 | （可能有）`clk-rk3588: assigned rate ... :` 仅出现在非 0 差异的钟；`unresolved` 不应出现 | assigned-clocks 应用情况：同值短路不碰硬件 |
| 3 | `serial: programming baud from <rate> Hz clock` 之后日志持续可读 | 控制台 baudclk（CRU id 187=SCLK_UART2）解析真值并重编程 8250 分频——消费侧闭环，P6 核心证据 |
| 4 | 无 `clk-rk3588: clock tree registration failed` / `lock timeout` / `malformed custom spec` | 事务注册与 PLL 序列健康 |

## 4. 对照排查（症状 → 首选怀疑）

| 症状 | 怀疑 | 验证 |
|------|------|------|
| `provider 0x2 ready` 缺失，`registration failed: InvalidParent` | 某 PNAME/父钟名翻译错 | 对照 data.rs 与 clk-rk3588.c 逐行（生成器机械等价）|
| `provider 0x2 ready` 但 `clk-rk3588: assigned clock <&0x2 N> unresolved` | 索引域错（mainline vs BSP）| 全部索引应来自 BSP 头；查 data.rs 中该 index |
| 串口乱码/停摆（切波特率后） | baudclk 解析到错误的钟（如 CLK_UART2_SRC 而非 SCLK_UART2）| 看 `programming baud from` 的 rate：应≈24MHz 整分频或 1.5M 整数倍 |
| gpll/cpll 速率错 | PLL recalc 的 m/p/s/k 位段 | `ldp check-layout --port rk3588-cru`（rk3588-pll-con0/1/2/6 已锚定参考）|
| 系统卡死在 `assigned rate`（改频中） | 同值短路失效 / PLL lock 超时恢复路径 | 找 `lock timeout` 与 `restoring old params` 日志 |

## 5. 实测结果（2026-10-27，Orange Pi 5 Plus 真机，P6 通过）

镜像：`xkernel_aarch64-rk3588.bin` @ commit 6ac016691（含 fixed-clock 按名应答、
CRU ClkProvider 门发布、Clk 类级 Deferred 唤醒三项修复）。

- CRU 全树注册：`provider 0x2 ready; gpll=1188000000Hz cpll=1500000000Hz
  npll=850000000Hz ppll=1100000000Hz aclk_top_root=594000000Hz
  pclk_top_root=100000000Hz sclk_uart2=24000000Hz` —— cpll=1.5G 与 DT
  assigned-clock-rates 一致（同值短路生效）。
- **消费端闭环**：dwmmc 从 CRU 解析 ciu=25MHz → SD 卡识别 → 4-bit 50MHz 高速
  → mmcblk0p1/p2 发布 → ext4 根文件系统挂载 → 用户态启动（Welcome to Kylin X!）。
- 4/19 assigned rate `NotSupported`（非阻塞，Linux 同样容忍）：
  HCLK_CENTER_ROOT(400M)、CLK_GPU(200M)、CLK_HDMIRX_AUD(99M)、
  PCLK_PMU0_ROOT(100M) —— 均为 mux 型复合钟，set_rate 需 parent 传播；
  后续如需精确设置再立 parent-propagation 增量。

## 6. 已知边界（如实标注）

- SCMI 相位钟（sdmmc ciu-drive/sample 706/707）解析 NotFound：SCMI provider 未移植。
- 复位（resets = <&cru …>）与重启 notifier 未移植：无 reset 框架/重启钩子。
- 22 个外部输入钟（mclkin/hdmiphy_pixel/pipephy 等）为 rate-0 占位，子树速率为 0，
  直到对应 provider 落地。
