# 驱动模型 v2:一次设备对象,分层对齐 Linux(完成态)

> 分支:`xiaohui/driver-model-v2`,基线:phase0(89c20be4e)。
> 三刀全落:devres 轴、类层归一、家族栈回家。本文件记录最终形态与已知债务。

## 最终结构

```
drivers/
  base/    driver-base —— 设备模型:device/driver/bus/class/domain + devres 前后端轴
           + class 机械(Publication/Registry/ClassHooks/class_api! 宏/notify+remove 引擎)
  host/    driver-host —— 资源后端(DeviceServices over kirq/kdma/memspace/kclk)
  init/    driver-init —— 组合根,显式 initcall 等级
  common/  devnode —— 节点模型 + NodeClass(input/drm 投影)+ uevent 格式
  subsystems/ subsys-* —— 每类完整栈:trait + class.rs(别名/钩子/发布 API/removal)+ dispatch
           (rtc = 第一个从"模型外"迁入的类:契约 + hctosys 服务)
  drv/     家族驱动 + 家族库(virtio/devices、serial/serial-core=UART 硬件、rtc=PL031/Goldfish/CMOS/LS7A)
  bus/     pci
  firmware/ of、acpi、dice(握手)、rs-fdtree(外部解析库)
  console/ printk 侧消费者(早启 + stdout 采用)
  clocksource/ irqchip/ perf/ —— 模型外内核集成驱动(启动序/架构钉死,见 drivers/README.md)
  integration/ uevent-netlink
arch/x86-apic —— CPU 中断基础设施(离开 drivers/)
```

## 三刀

1. **devres 一根轴**:`base/src/devres/` = 前端守卫(iomap/irq/dma/clk)+ 后端能力
   (provider.rs)+ 缝合面(services.rs);`resource.rs` 固件侧数据。
2. **类机械归一**:`base/src/class/` 拥有全部通用机械;一个类 = 一个
   `pub static X: Class<T, H>`,驱动用 `dev.publish(&X, runtime, name)` 发布,
   返回的 `Publication<T>` 就是所有者(drop 即撤销,`InUse` 计数让 class 侧
   teardown 等在途操作结束);钩子 `H: ClassHooks<T>` 是类型参数。设备身份是
   域创建的 `DeviceCore`,不再有 `ClassParent` 包装;devnode 投影随
   `Publication` 的发布/撤销走。
3. **家族栈回家**:转发 impl(消费面)住在各子系统;发布/订阅/查找 API 住在各
   子系统;节点投影词汇在 devnode。孤儿规则与 initcall 等级(L1-L5)保证序。
4. **模型外驱动迁入(RTC 首例)**:`drivers/rtc` 拆为 `subsystems/rtc`
   (RtcDevice 契约 + RTC 类 + `hctosys` 服务)+ `drv/rtc`(PL031/Goldfish/
   CMOS/LS7A 四个平台总线驱动);墙钟在 L5 probe storm 之后由组合根播种
   (Linux late_initcall(rtc_hctosys) 序),平台 `early_driver_init` 不再直接读 RTC。

## 验证

aarch64 build/clippy 0 error、全部单测通过、QEMU 启动到 shell、
riscv64/x86_64 交叉构建 0 error。

## 下半场(已完成):DeviceCore 值桥 → `DeviceCore` 活引用

`DetachedParent`(owner/driver_name/physical_path/offline 四项值拷贝)消亡。
域在 `add_device` 创建 `Arc<DeviceCore>`——设备的活身份卡:id、驱动名(probe
开始时由域写入)、物理路径(总线词汇一次性派生)、offline 标志。每个
`DeviceCore::Device` 持有它;发布点从 `DeviceCore::detached(dev.id().raw(),
"硬编码名", location)` 变为 `dev.publish(&CLASS, …)`——12 个发布点的硬编码驱动名
与手工 location 全部消亡。一设备多发布共享同一 offline:设备开始拆除的瞬间,
它所有发布一起观察到不可用(比旧语义更正确)。

## 已知债务(后续刀)

1. `drivers/{console,clocksource,irqchip,perf}` 仍不在模型内:clocksource/
   irqchip 被启动序钉在 `driver-init` 之前(irqchip 在 `kirq` 之下),perf 是
   架构 PMU、尚无 perf 类。console 已完成到"char 类消费者"的一半,可参照
   RTC 的拆法继续收敛。
2. `drv/serial/serial-core` 内嵌 `input.rs`(串口输入环)与 devnode 语义耦合。
3. `virtio_hal`/`virtio_transport` 仍在 host 顶层文件(家族区未显性子目录化)。
4. kfeat/kruntime 的 feature 网与新 initcall 等级并存(等级是序,feature 是集;
   未来可把 feature 收敛为纯选择)。
