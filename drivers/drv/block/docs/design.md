# drv-block — 设计文档

## 定位

driver-core 模型下的块设备驱动聚合 crate。一个模块一个驱动，一个 feature 一个
驱动（Linux 的 file-per-driver 模型）。

当前只有一个驱动：`ramdisk`。它是**最简完整模板**——板级静态设备、无固件资源、
按 alias 认领、发布一个块设备。转写新驱动时先看它，再看 `drv/virtio/src/blk.rs`
（PCI + IRQ + devnode 的完整形状）。

## 范围

| 文件 | 内容 |
|---|---|
| `src/lib.rs` | crate 门面（`mod common; pub mod ramdisk;`） |
| `src/common.rs` | `map_driver_error`（`KError` → `ProbeError::Fatal(RuntimeError)`）、`physical_path`（转发 `devnode::physical_path`） |
| `src/ramdisk.rs` | `RamdiskDriver` + `RamdiskData` + `IDS` + `register()` |

## 架构

```text
driver-compose::enumerate_static_devices()
   └─ DeviceDescriptor { location: PlatformStatic{id:0},
                         identity: Platform{ alias: Some("ramdisk"),
                                             compatibles: [] } }
        │ add_device → probe
        ▼
RamdiskDriver::probe(dev)
   ├─ cfg(ramdisk-static) → block::ramdisk_image::ramdisk()   // 构建期内嵌镜像
   │  否则                → block::ramdisk_heap::RamDisk::new(KFEAT_RAMDISK_SIZE)
   ├─ Gendisk::new(name, major=1, first_minor=0, minors=1, dev)
   │      .with_devnode_context(parent_path, modalias)
   ├─ device_class::publish_block(ClassParent::detached(dev.id().raw(), "ramdisk", location), Arc::new(disk))
   └─ Ok(RamdiskData { _publication: device_class::block_withdraw(&publication) })
```

板级静态设备**没有 compatible**（`compatibles: Vec::new()`），因此按
`PlatformId::Alias(AliasMatch::new("ramdisk"))` 认领，不是 of-match。

## 调用约束 / 执行上下文

- `probe` / `remove` 由域在**锁外的任务上下文**调用，可睡眠。
- `register()` 只在 `driver-compose::init()` 里调用一次；它必须在
  `block::dispatch::reclaimer::init()` 之后（compose 把两行放在同一个
  `#[cfg(feature = "ramdisk")]` 块里）。
- 本 crate 无锁、无原子、无中断处理。块 I/O 的完成与回收全在
  `subsystems/block::dispatch`。

## 设计决策

### `ramdisk-static` 用 `#[cfg]` 而不是 `if cfg!()`

两个分支产生**不同的具体类型**（`ramdisk_image` 的只读镜像设备 vs
`ramdisk_heap::RamDisk`），`if cfg!()` 要求两边类型一致，所以只能用属性。
两个分支都显式标注 `(Box<dyn BlockDeviceOperations + Send + Sync>, String)`，
避免类型推断在只编译一边时漂移。

### 容量是构建参数，不是固件属性

`KFEAT_RAMDISK_SIZE`（默认 `0x1000000`）对应 Linux brd 的 `rd_size`。合成设备
没有固件描述可读，所以走 Kconfig。

### `with_devnode_context` 是必需的，不是可选的

没有它，`add_disk` 不会投影 `/dev/ramdisk`、不发 uevent、不建 sysfs 目录——
因为父路径未设置。这是**每个块驱动都要遵守的同一契约**（virtio-blk 相同），
不是 ramdisk 的特殊处理。

## Drop / 资源释放

`RamdiskData` 只有一个字段：`_publication: device_class::ClassWithdraw`。
它是**第一个也是唯一一个**字段，drop 时触发
registry removal → `del_gendisk` → devnode withdraw。

`remove()` 是空的——不是遗漏：ramdisk 没有需要 quiesce 的硬件，也没有 IRQ。
框架在 `remove()` 返回后 drop `Data`，发布随之撤销。
（对比 `drv/virtio/src/blk.rs`：那里有真实硬件，`remove()` 必须显式
drain I/O → del_gendisk → 停中断 → 停 reclaimer。）

`Gendisk` 本身由 class registry 持有（`Arc<Gendisk>`），`RamdiskData` 不持有
它的强引用——撤销发布即释放。
