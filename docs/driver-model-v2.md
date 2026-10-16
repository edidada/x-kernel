# 驱动模型 v2:一次设备对象,分层对齐 Linux(完成态)

> 分支:`xiaohui/driver-model-v2`,基线:phase0(89c20be4e)。
> 三刀全落:devres 轴、类层归一、家族栈回家。本文件记录最终形态与已知债务。

## 最终结构

```
drivers/
  base/    driver-base —— 设备模型:device/driver/bus/class/domain + devres 前后端轴
           + class 机械(ClassDevice/Registry/ClassHooks/class_api! 宏/notify+remove 引擎)
  host/    driver-host —— 资源后端(DeviceServices over kirq/kdma/memspace/kclk)
  init/    driver-init —— 组合根,显式 initcall 等级
  common/  devnode —— 节点模型 + NodeClass(input/drm 投影)+ uevent 格式
  subsystems/ subsys-* —— 每类完整栈:trait + class.rs(别名/钩子/发布 API/removal)+ dispatch
  drv/     家族驱动 + 家族库(virtio/devices、serial/serial-core=UART 硬件)
  bus/     pci
  firmware/ of、acpi、dice(握手)、rs-fdtree(外部解析库)
  console/ printk 侧消费者(早启 + stdout 采用)
  clocksource/ irqchip/ rtc/ perf/ —— 模型外内核集成驱动
  integration/ uevent-netlink
arch/x86-apic —— CPU 中断基础设施(离开 drivers/)
```

## 三刀

1. **devres 一根轴**:`base/src/devres/` = 前端守卫(iomap/irq/dma/clk)+ 后端能力
   (provider.rs)+ 缝合面(services.rs);`resource.rs` 固件侧数据。
2. **类机械归一**:`base/src/class/` 拥有全部通用机械;`ClassHooks`(函数指针,
   Linux struct class 字面形状)替代 trait 钩子;8 个家族的实例化各自住在
   `subsystems/*/src/class.rs`(9p 在 virtio-devices)。device-class crate 死亡。
   metadata 袋死亡(input 身份由运行时投影时查询)。
3. **家族栈回家**:转发 impl(消费面)住在各子系统;发布/订阅/查找 API 住在各
   子系统;节点投影词汇在 devnode。孤儿规则与 initcall 等级(L1-L5)保证序。

## 验证

aarch64 build/clippy 0 error、全部单测通过、QEMU 启动到 shell、
riscv64/x86_64 交叉构建 0 error。

## 已知债务(后续刀)

1. **ClassParent 仍是值桥**(owner/driver_name/physical_path 快照 + offline 标志):
   Device↔ClassDevice 对象反转未做——需要 domain 核心改 Arc 共享设备条目。
   这是"两个设备对象"病根的最后半场。
2. `drv/serial/serial-core` 内嵌 `input.rs`(串口输入环)与 devnode 语义耦合。
3. `virtio_hal`/`virtio_transport` 仍在 host 顶层文件(家族区未显性子目录化)。
4. kfeat/kruntime 的 feature 网与新 initcall 等级并存(等级是序,feature 是集;
   未来可把 feature 收敛为纯选择)。
