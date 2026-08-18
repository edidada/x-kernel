# x-kernel Fuzz 入口清单

本文整理当前仓库适合作为 fuzz harness 的输入面，供扩展 `fuzz/`（WFUZZ）与后续
stateful / syscall / in-guest 轨道规划使用。

口径：

- **入口**：建议单独维护的 fuzz target 名 + 被测 API。
- **类型**：`unit`（`&[u8]` → 解析/校验）/ `stateful` / `syscall` / `in-guest`。
- **就绪**：`done` / `ready`（可 path-dep）/ `export`（需导出 API）/
  `extract`（需抽 crate）/ `defer`（需重型 harness）。
- **价值**：相对安全/稳定性收益的粗估（`high` / `med` / `low`）。

粗估：忽略工程成本时全仓约有 **150–220** 个可区分 fuzz 面；下表列出可落地的
代表入口。现有 harness 见 [`fuzz/README.md`](../fuzz/README.md)。

原则：优先 fuzz「不可信 `&[u8]` → 解析/校验」的独立 crate；避免整仓 path-dep
`knet` / `kvfs` / `ksyscall`。

---

## 字段说明

| 字段 | 含义 |
|------|------|
| 入口名 | 建议的 `fuzz_targets/<name>.rs` / WFUZZ entrypoint 名 |
| 模块 | 源码路径或 crate |
| API | 主要被测入口 |
| 类型 | unit / stateful / syscall / in-guest |
| 就绪 | done / ready / export / extract / defer |
| 价值 | high / med / low |
| 备注 | 前置条件、corpus、注意事项 |

---

## 0. 已有

| 入口名 | 模块 | API | 类型 | 就绪 | 价值 | 备注 |
|--------|------|-----|------|------|------|------|
| `fdt_parse` | `drivers/rs-fdtree` | `LinuxFdt::new` + traverse nodes/props | unit | done | high | !555；corpus：`drivers/rs-fdtree/dtb/` |
| `fdt_typed_helpers` | `drivers/rs-fdtree` | chosen / memory / irq / dice / path resolve | unit | done | high | 加深 typed helpers |
| `kernel_elf_parse` | `boot/kernel-elf-loader` | `KernelElf::parse` → segments/symbols | unit | done | high | 零依赖 boot ELF |
| `user_elf_parse` | `boot/kernel_elf_parser` | `ELFHeadersBuilder` → `ELFParser` | unit | done | high | alloc + xmas-elf |
| `kbpf_vm_run` | `core/kbpf` | `Vm::with_instruction_limit` + `run` | unit/stateful | done | med | limit=256 |
| `rsext4_extent_parse` | `fs/filesystems/rsext4` | `ExtentTree::parse_node` | unit | done | high | |
| `rsext4_dirent_parse` | `fs/filesystems/rsext4` | dirent parse + iterator | unit | done | high | |
| `rsext4_gdt_walk` | `fs/filesystems/rsext4` | `BlockGroupDescTable` | unit | done | high | |
| `rsext4_sb_decode` | `fs/filesystems/rsext4` | `Ext4Superblock::from_disk_bytes` | unit | done | high | 需 ≥1024 字节 |
| `rsext4_inode_decode` | `fs/filesystems/rsext4` | `Ext4Inode::from_disk_bytes` | unit | done | med | 需 ≥128 字节 |
| `firmware_handoff_memmap` | `boot/firmware-handoff` | `BootMemmapRef::from_bytes` | unit | done | med | 新增 safe `&[u8]` API |
| `fs9p_dirent` | `fs/filesystems/fs9p` | `parse_dir_entries` / `_l` | unit | done | med | `feature = "fuzz"` |
| `fs9p_path` | `fs/filesystems/fs9p` | `path_parts` / `split_parent_name` | unit | done | med | `feature = "fuzz"` |
| `tee_sm2_sec1` | `tee/tee_crypto` | `sm2_validate_sec1_public_key` | unit | done | med | |
| `tee_rsa_der` | `tee/tee_crypto` | RSA PKCS#1 / SPKI DER | unit | done | med | |

当前 harness：**15** 个入口（相对文档近/中程可落地清单约 **20%**；全仓粗估 150–220 面约 **7–10%**）。

---

## 1. 近程（WFUZZ / 现有 harness）

可直接或几乎直接 path-dep 进独立 `fuzz/` workspace。

| 入口名 | 模块 | API | 类型 | 就绪 | 价值 | 备注 |
|--------|------|-----|------|------|------|------|
| `boot_info_validate` | `boot/boot-info` | `BootInfo::is_valid` / cmdline / e820 | unit | export | med | bootloader ABI；deps 偏重 |
| `uefi_config` | `boot/x86_64-uefi-loader` | `parse_config` / sanitize path | unit | export | low | 文本配置，信任路径偏多 |
| `build_info_decode` | `boot/kernel-image-metadata` | `decode_build_info` | unit | defer | low | main 才有该 crate；release/2606 暂无 |

---

## 2. 中程：导出后 unit-fuzz

API 多为 `pub(crate)` / private；加 `feature = "fuzz"` 或薄导出后再进 WFUZZ。

### 2.1 kext4 on-disk（建议拆多入口）

模块：`fs/filesystems/kext4`。先 fuzz decode，**不要**先 fuzz `mount`。

| 入口名 | API | 类型 | 就绪 | 价值 | 备注 |
|--------|-----|------|------|------|------|
| `kext4_sb_decode` | `Superblock::decode` | unit | defer | high | decode 已 `pub`，但 path-dep 拉 `karch`/`kbuild_config`，WFUZZ 宿主编不过 |
| `kext4_gdt_decode` | group descriptor `decode` | unit | defer | high | 同上 |
| `kext4_inode_decode` | inode `decode` | unit | defer | high | release/2606 disk/ 尚无 |
| `kext4_extent_decode` | extent header/index/leaf `decode` | unit | defer | high | 同上 |
| `kext4_extent_validate` | `validate_extent_entries` | unit | defer | high | 同上 |
| `kext4_dir_decode` | dirent / htree `decode` | unit | defer | high | 同上 |
| `kext4_xattr_decode` | xattr header/entry `decode` | unit | defer | high | 同上 |
| `kext4_jbd2_sb` | jbd2 superblock `decode` | unit | defer | high | 同上 |
| `kext4_jbd2_log` | `parse_header` / tags / revoke | unit | defer | high | 同上 |

Corpus 线索：`fs/filesystems/kext4/tests/linux_image.rs`、disk 单元测试。

### 2.2 其它 FS / 驱动解析

| 入口名 | 模块 | API | 类型 | 就绪 | 价值 | 备注 |
|--------|------|-----|------|------|------|------|
| `fs9p_dirent` | `fs/filesystems/fs9p` | `parse_dir_entries` / `_l` | unit | done | med | `feature = "fuzz"` |
| `fs9p_msg` | `fs/filesystems/fs9p` | `message::read_*` / qid / str | unit | ready | med | 可并入 dirent harness |
| `acpi_resource_tpl` | `drivers/acpi` | `parse_resource_template_for_mem_window` | unit | export | med | AML `_CRS`；避开 RSDP/MMIO |
| `pci_msix_layout` | `drivers/pci` | `validate_msix_layout` | unit | export | med | capability 边界 |
| `serial_regwidth` | `drivers/console` | `SerialRegWidth::from_bytes` | unit | export | low | DT `reg-io-width` |
| `proc_oom_adj` | `fs/filesystems/procfs` | `parse_oom_score_adj_input` | unit | export | low | proc 写入 |
| `path_normalize` | `fs/kvfs` | `Pathname::normalize` | unit | extract | med | 建议抽薄 path 模块；输入为 UTF-8 路径 |

### 2.3 TEE / crypto

| 入口名 | 模块 | API | 类型 | 就绪 | 价值 | 备注 |
|--------|------|-----|------|------|------|------|
| `tee_postcard_msg` | `tee/tee_kernel` | `decode_message` | unit | extract | med | Host↔TEE；2606 偏 bincode + 重依赖 |
| `tee_path_uuid` | `tee/tee_kernel` | path / UUID validate | unit | export | med | 字符串规则 |
| `cbor_dice` | `tee/xdice` | `CborIn::read_*` / skip | unit | defer | med | release/2606 无此 crate |
| `pkix_der` | `tee/tee_crypto` | DER/pkix decode | unit | export | med | 依赖 feature `pkix` |
| `sig_blob_parse` | `tee/tee_crypto` | SM2/Ed25519 sig parse | unit | export | med | crypto blob |
| `tee_sm2_sec1` | `tee/tee_crypto` | `sm2_validate_sec1_public_key` | unit | done | med | 已落地 |
| `tee_rsa_der` | `tee/tee_crypto` | RSA PKCS#1 / SPKI DER | unit | done | med | 已落地 |

---

## 3. 中程：需抽 crate（网络 wire）

**不要**把整个 `net/knet` path-dep 进 `fuzz/`。先抽出 `knet-wire` / `netlink-wire`，
或 `feature = "fuzz"` 导出 `pub(super)` 解析器。

| 入口名 | API | 类型 | 就绪 | 价值 | 备注 |
|--------|-----|------|------|------|------|
| `nl_msg_header` | `NlMsgHeader::read` | unit | extract | high | `net/knet/src/netlink/wire.rs` |
| `nl_parse_attrs` | `parse_attrs` | unit | extract | high | TLV |
| `nl_ifinfo` | `IfInfoMsg::read` | unit | extract | high | |
| `nl_ifaddr` | `IfAddrMsg::read` | unit | extract | high | |
| `nl_rtmsg` | `RtMsg::read` | unit | extract | high | |
| `nl_ndmsg` | `NdMsg::read` | unit | extract | high | |
| `rtnl_parse_link` | `parse_link_*` | unit | extract | high | security.md 相关面 |
| `rtnl_parse_addr` | `parse_addr_*` | unit | extract | high | |
| `rtnl_parse_route` | `parse_route_*` | unit | extract | high | |
| `rtnl_parse_neigh` | `parse_neigh_*` | unit | extract | high | |
| `eth_frame` | `EthernetFrameRef::new_checked` | unit | extract | med | L2 |
| `arp_parse` | ARP `parse` | unit | extract | med | |
| `ipv4_header` | `Ipv4Header::parse_input` | unit | extract | med | 注意与 etherparse 边界 |
| `udp_datagram` | `parse_udp_datagram` | unit | extract | med | |
| `raw_ip_packet` | `parse_ip_packet` | unit | extract | med | |
| `ifname_mtu` | `validate_interface_name` / MTU | unit | extract | low | 策略校验 |
| `vsock_service_name` | `parse_service_name` | unit | export | low | vsock↔TIPC |

Corpus 线索：`net/knet/src/netlink/tests.rs`。

---

## 4. 后期：stateful / syscall / in-guest

| 入口名 / 面 | 模块 | API | 类型 | 就绪 | 价值 | 备注 |
|-------------|------|-----|------|------|------|------|
| `kext4_mount_io` | `kext4` | `Ext4Filesystem::mount` + dir/file ops | stateful | defer | high | BlockDevice mock + journal |
| `ip_reassembly` | `knet` | fragment reassembly | stateful | defer | high | 重组状态机 |
| `netlink_full` | `knet` | `send_with_cred` / handle_rtnetlink | stateful | defer | high | 全局 SERVICE / devices / creds |
| `packet_ingress` | `knet` | `IngressProcessor` RX | stateful | defer | high | smoltcp SocketSet |
| `kbpf_with_maps` | `kbpf` / posix-bpf | VM + maps/helpers | stateful | defer | med | 待 verifier/maps 成熟 |
| `kexec_load` | `process/kexec` | `load_user_app_request` | stateful | defer | high | VFS + process + TEE |
| `tipc_queue` | `tee/tipc` | msg queue push/read | stateful | defer | med | kprocess/ktask |
| `ta_elf_verify` | `tee_task_iface` | TA ELF verify | stateful | defer | med | 签名镜像 |
| `sys_bpf_attr` | `posix/bpf` | `bpf_attr` / `BPF_PROG_LOAD` | syscall | defer | med | kuaccess / FD |
| `cmsg_parse` | `posix/net` | `CMsg::parse` | syscall | defer | med | ancillary |
| `sockaddr_abi` | `posix/net` | sockaddr load/validate | syscall | defer | med | |
| `clone3_args` | `ksyscall` | `clone_args` | syscall | defer | med | |
| `sys_*` 族 | `ksyscall` / `posix/*` | ~256 `sys_*` | syscall | defer | high | 按族拆或 guest fuzz |
| `drm_ioctl` | `io/drmdevice` | DRM ioctl 族 | in-guest | defer | med | |
| `tty_ioctl` | `io/ktty` | termios / winsize | in-guest | defer | med | |
| `devfs_ioctl` | `devfs` | loop/fb/event/rtc/dice | in-guest | defer | med | |
| `virtio_config` | `drivers/virtio` | config space / features | in-guest | defer | med | MMIO/HAL |
| `virtio_queues` | `drivers/virtio` | 9p/net/blk framing | stateful/in-guest | defer | med | 队列状态 |
| `of_init` | `drivers/of` | `init` + GIC/PCI helpers | defer | defer | med | 全局 `LazyInit<LinuxFdt>` |

---

## 5. 明确低优先 / 暂不做

| 项 | 原因 |
|----|------|
| 单独 fuzz `Insn::from_bytes` | 几乎总成功，无拒绝路径 |
| 纯第三方：`etherparse` / `xmas-elf` / `fatfs` | 优先 fuzz 第一方 glue/边界 |
| `util/xcover` `parse_profraw` | 覆盖率工具面，非运行时攻击面 |
| `fs/fat` 薄封装 | 价值在上游 `fatfs` |
| UEFI `parse_config` 当安全 P0 | 偏信任启动配置 |
| `build_info_decode` 当安全 P0 | 易做但攻击面小，可作 smoke |

---

## 6. 推荐落地顺序

1. ~~`kernel_elf_parse` + `fdt_typed_helpers` + `user_elf_parse`~~（已落地）
2. ~~`kbpf_vm_run` + `rsext4_*` + `firmware_handoff_memmap` + `fs9p_*` + `tee_{sm2,rsa}`~~（已落地，近/中程清单 ~20%）
3. knet-wire 抽取；kext4 需拆无 `karch` 的 disk-only crate 后再进 WFUZZ
4. stateful / syscall / in-guest 另开轨道
5. TEE：`pkix_der` / path UUID（在 deps 允许时）
6. （保留）`boot_info_validate` 等偏重 deps 项

---

## 7. Harness 约定（扩展 `fuzz/` 时）

与现有 WFUZZ 布局对齐（详见 `fuzz/README.md`）：

```text
fuzz/
├── Cargo.toml              # 独立 [workspace]，path-dep 被测 crate
├── fuzz_targets/<name>.rs
├── wfuzz.json
├── fuzz_build.sh
└── .cargo/config.toml      # 仅影响 fuzz 构建
```

建议：

- 每个入口一个 `[[bin]]`，命名与上表「入口名」一致。
- 输入尽量是纯 `&[u8]`；拒绝路径用 `return`，避免在 harness 里 `unwrap` 合法错误。
- 有状态执行（如 `Vm::run`）必须设资源上限。
- Corpus 优先复用树内测试固件；生成物放 `fuzz/` 且保持 `.gitignore`。

---

## 8. 现成 corpus 线索

| 来源 | 可用于 |
|------|--------|
| `drivers/rs-fdtree/dtb/` | FDT |
| `boot/kernel_elf_parser/tests/elf_*` | user ELF |
| `fs/filesystems/kext4/tests/` / disk 单测 | kext4 decode |
| `net/knet/src/netlink/tests.rs` | netlink wire |
| `core/kbpf/src/vm.rs` 测试程序 | kbpf |

---

## 修订记录

| 日期 | 说明 |
|------|------|
| 2026-08-14 | 初稿：基于 `fuzz/`（!555）与全仓 parse/decode/syscall 面梳理 |
| 2026-08-14 | 落地近程 unit：`fdt_typed_helpers` / `kernel_elf_parse` / `user_elf_parse`（`release/2606`；无 `build_info_decode`） |
| 2026-08-14 | 扩到 15 入口：kbpf / rsext4×5 / firmware memmap / fs9p×2 / tee×2（近/中程清单 ~20%；kext4 因 karch 暂 defer） |
