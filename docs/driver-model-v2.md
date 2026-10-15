# 驱动模型 v2:一次设备对象,分层对齐 Linux(MVP 状态)

> 分支:`xiaohui/driver-model-v2`,基线:phase0(89c20be4e)。
> 本文档是 v2 重构的**当前状态**与**剩余路线**,不是完成宣言。

## 目标

一个设备对象模型 + Linux 可读的分层与初始化流程,消灭三条历史缝合线:

1. **资源获取无轴**:resource/provider/services/handle 四个名字讲一件事 → `devres` 前后端一根轴
2. **双设备对象**:driver-core 的 Device 与 device-class 的 ClassDevice 并行、ClassParent 值拷贝桥 → 类机械并入 base,一个 crate 讲完设备故事
3. **子系统空心**:trait 在 subsystems、发布在 device-class、转发在 device-class → 转发随孤儿规则回归子系统,子系统拥有自己的消费面

## 已落地(MVP,已验证 build/clippy/unittest/boot)

| 变更 | 内容 |
|---|---|
| 目录/包 | `drivers/core/{driver-core,driver-host,driver-compose}` → `drivers/{base,host,init}`;包名 `driver-base`/`driver-host`/`driver-init`;消双重前缀与内核 `core/` 撞名 |
| devres 轴 | `base/src/devres/`:前端守卫(iomap/irq/dma/clk)+ 后端能力(provider.rs:IrqOp/MmioOp/DmaOp)+ 缝合面(services.rs:DeviceServices);`resource.rs` 留固件侧数据 |
| 类机械并入 | `base/src/class/`:ClassDevice/ClassRegistry/ClassParent/Metadata/Withdraw —— 模型 crate 拥有类机械;`device-class` 收缩为家族实例化层(class_api! 宏、8 个家族、removal 家族分支、node 投影) |
| **转发回归子系统** | 孤儿规则强制出的正确分层:`impl XDevice for ClassDevice<Impl>` 住在各 subsystem crate(char/display/input/net/vsock/hwrng)+ virtio-9p 在家族库。子系统依赖 driver_base(= Linux drivers/block 依赖 struct device)。**consumer 面由子系统自持** |
| initcall 等级 | `driver-init::init()` = L1 host services → L2 subsystem cores → L3 drivers → L4 enumeration → L5 probe storm;cargo feature 只选"谁编进来",等级决定"谁先跑" |

## 剩余路线(pass 2+,按价值排序)

1. **Device↔ClassDevice 对象反转**(核心手术的下半场):ClassDevice 从 Arc 平行对象变成 Device 成员资格的视图;ClassParent 值桥删除,发布挂真父。消费者面(ClassDevice<T> handle)保持不变,所有权反转
2. **console/serial 切分**:UART 硬件(SerialPort/后端)从 `platform/console` 抽为 serial 家族库(console 与 drv-serial 共同的下层);console 收缩为 printk 侧消费者。依赖方向:需新 crate 于两者之下,解掉 drv-serial → console-driver 的倒挂
3. **platform/ 杂物间**:x86-apic → arch 侧;dice → 非 drivers;timer → clocksource 命名;三时钟世界消歧
4. **Impl 别名 + node 投影归子系统**:`*DeviceImpl` 别名、NodeClass/InputNodeMetadata、node_compat 从 device-class/base 移交各子系统(L0-a 债:base::class 的 InputIdentity 袋)
5. **包命名统一**:现存三套前缀(driver-*/drv-*/*-driver)+ 裸名词(net/block 冲突)+ char_driver 孤例;统一为一套 `<域>-<名>`
6. 块子系统栈补完(Gendisk/add_disk/投影一条线收回 subsystems/block)

## 验证

- aarch64 build/clippy 0 error;3348 单测全过;QEMU 启动到 shell
- 消费者(ktty/devfs/knet/console/entropy)零改动 —— ClassDevice 表面保持兼容,内部所有权迁移
