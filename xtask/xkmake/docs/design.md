# xkmake — Design Document

## Positioning

`xkmake` is the build and run orchestrator inside the X-Kernel repository. The root `Makefile` only provides
a stable entry point; `xkmake` is responsible for connecting `xconfig`, Cargo, the image processing tools, and QEMU.

## Scope

```text
src/
├── main.rs       command dispatch
├── cli.rs        strongly typed command-line arguments
├── context.rs    final build context
├── build.rs      Cargo, images, and bundle
├── linker.rs     kernel linker script rendering and stable writing
├── coverage.rs   unittest coverage extraction and report generation
├── doc.rs        workspace rustdoc orchestration
├── hygiene.rs    仓库卫生工具编排
├── dylint.rs     Dylint lint library build and kernel scan
├── facts.rs      packing of the code facts exported by the Dylint scan
├── image_metadata.rs  内嵌构建信息与 Build ID 的 ELF finalize
├── qemu.rs       平台启动策略
├── x86.rs        LinuxBoot/UEFI 启动介质
├── process.rs    外部进程边界
├── dwarf_embed.rs 进程内 DWARF 段嵌入（KFEAT_DWARF 时启用）
├── symtab.rs     紧凑内核符号表生成（KFEAT_SYMTAB）
├── symbolize.rs  日志地址的 Host 侧符号化
└── error.rs      工具层错误模型
```

## Architecture

```text
Makefile
   |
   v
xkmake ------> xconfig ------> .config / config.rs / Cargo config
   |
   +---------> Cargo --------> kernel ELF
   |                            |
   |                            +--> kernel.debug.elf（完整符号、DWARF 和覆盖率映射）
   |
   +-- embed DWARF on demand (KFEAT_DWARF) / bootstrap symbol table (KFEAT_SYMTAB)
   |      + rust-objcopy (external)
   |                         |
   |                         v
   +--------------------> versioned bundle
                               |  \
                               |   +--> workspace-root xkernel_* copies
                               |
                               +--> make symbolize (host-side recovery of function names and line numbers)
                               |
                               v
                              QEMU
```

The conversion from LCOV to Cobertura XML is an internal stage of the unittest coverage flow; it does not expose
a separate command or a second argument model.

`xconfig::build_config::ResolvedKernelConfig` is the stable semantic
boundary between the configurator and the builder. `xkmake` does not read the `.config` text, nor does it derive
the architecture, platform, or features back from the generated TOML.

## Core Flows

### Build

1. 校验当前目录是 X-Kernel workspace。
2. 通过一次 `ConfigEngine` 求值生成完整的 `ResolvedKernelConfig` 快照。
3. 从该快照生成 `.config`、`auto.conf`、`autoconf.h`、`config.rs`、
   rust-analyzer 设置和 Cargo 配置，并从仓库根目录
   `linker.lds.S` 渲染当前 target 的 linker script；生成阶段不再次
   解析 Kconfig。linker script 只在
   内容变化时重写，避免无效更新 mtime 使 Cargo 重新编译。
4. 构造一次性的 `BuildContext`，之后不再读取环境或配置文件。
5. 调用 Cargo 生成目标 ELF。
6. x86_64 额外增量构建 boot stub 与 EFI loader。
7. 若现有 bundle 的稳定构建来源、配置 hash、输入时间、产物名称、大小和
   mtime 顺序仍匹配，则作为可信本地缓存直接复用，并保留原有构建时间；
   普通 build 不重新扫描完整 ELF。
8. 否则在临时路径保存完整 Cargo ELF 供 Host 分析；启动 ELF 按需嵌入
   DWARF（KFEAT_DWARF），再由 `rust-objcopy --strip-all` 移除非加载调试与
   覆盖率映射。x86 保留引导器查找的 `rust_entry` 和 `boot_stack_top`。
   最后原地填充 linker 预留的 `.note.xkernel.build-info` 与 `.note.gnu.build-id`；自动
   构建时间在此时取当前 UTC 时间，而不是参与前置缓存判断。
9. Build ID 对 build-info 已写入、Build ID descriptor 清零后的全部
   `PT_LOAD` 内容计算 SHA-256。
10. 从 finalized ELF 生成 raw BIN。
11. x86_64 在进程内生成 LinuxBoot header/镜像，并创建 UEFI FAT 镜像。
    LinuxBoot 的 i386 setup 代码用 GNU 工具汇编链接：x86_64 主机用本机
    `gcc`/`ld`/`objcopy`，其他主机用 `x86_64-linux-gnu-*`。
    FAT 镜像按引导器、内核 ELF 和配置文件的实际大小计算，预留两份 FAT、
    目录和分配对齐空间，按 MiB 对齐且至少 256 MiB，支持较大的 UT/coverage 产物。
12. 移除旧 manifest，提升所有产物，最后提升新 manifest 作为有效性标记。
13. 从有效 bundle 刷新仓库根目录的 `xkernel_<platform>.*` 兼容产物，供
    Jenkins 制品传递和仍使用旧命名的工具消费。复制先写入不存在的同目录
    临时路径，使 APFS/Linux CoW 文件系统可走 clone/reflink 快速路径，再
    以 rename 替换最终文件；文件系统不支持时由标准库回退普通复制。

### Artifacts

Each bundle contains two kinds of ELF:

- `kernel.elf`：精简的启动 ELF，保留加载段及 x86 必需的两个引导符号。
  KFEAT_DWARF 时嵌入的可加载调试段、KFEAT_SYMTAB 的运行时符号表，以及
  unittest 的运行时 profile 数据和计数器均保留；
- `kernel.debug.elf`：**始终**是未加工的 Cargo 产物（非 alloc `.debug_*`
  及 `__llvm_covfun` / `__llvm_covmap` 完整保留），供 Host 符号化、GDB、
  反汇编和覆盖率报告使用。`config bundle-debug-elf` 提供该产物的路径。

LinuxBoot 和 UEFI 都封装 `kernel.elf`；文件中的 Host 调试数据不再进入
启动时的源镜像保留区。bundle 格式 v8 强制重建旧版本产物，避免复用
尚未精简的启动 ELF。

### Symbol Table Bootstrapping (KFEAT_SYMTAB)

The compact symbol table (kallsyms style) comes from the kernel's own symbols, so there is a circularity: "the table needs the link output,
and the output needs the table". The build converges in two rounds:

1. The first round links with an empty table; `create_bundle` extracts function symbols from the Cargo ELF and writes them to
   `$TARGET_DIR/kbuild/ksymtab.bin`;
2. When the content has changed, Cargo is invoked again to relink (`util/backtrace` copies
   that blob through build.rs and embeds it with `include_bytes!`), and then the bundle is rebuilt.

Convergence is guaranteed by the layout: the blob lives in `.rodata` (an ordinary static), the symbol table only records functions in
the `.text` family of sections, and `.text` comes before `.rodata`, so a change in the blob size
does not change the addresses recorded in the table. The blob in the second round is identical to that of the first round, and the build stops.

With KFEAT_SYMTAB=n, an empty 16-byte blob is likewise written to disk. The build.rs of `util/backtrace`
tracks `$TARGET_DIR/kbuild/
ksymtab.bin` with `cargo:rerun-if-changed`, while Cargo treats any nonexistent `rerun-if-changed` input as
dirty, which would cause backtrace and all of its transitive dependencies to be fully recompiled on every build.
Therefore xkmake prepares this input before the first Cargo invocation: when KFEAT_SYMTAB is off it writes
a stable empty blob; when it is on it keeps the existing symbol table and writes an empty blob only if the file is missing.
In this way, after a clean build finishes, the input going from missing to present does not trigger an extra next build,
and subsequent builds can fully hit the Cargo incremental cache.

### Symbolize

`make symbolize LOG=<log>` extracts the raw addresses of the `Backtrace:` block from the log (compatible with the
`func+0xoff/0xsize` annotations), and calls `llvm-addr2line`/`addr2line` against
`kernel.debug.elf` to output function names and file/line numbers. `--offset` supports a kernel load address
offset (such as KASLR).

`make run`/`make justrun` symbolize automatically: QEMU output is tee'd to the terminal in real time and mirrored to
`bundle/qemu.log`; after the run ends, the backtrace frames are scanned, and if any are found they are automatically resolved against
`kernel.debug.elf` and printed (`--no-symbolize` turns this off). The tee is implemented by
`Process::run_tee`: stdout goes through a pipe and is forwarded by a reader thread to the terminal and the log file,
while stdin/stderr remain inherited, so interactive sessions are unaffected.

### Run

1. Execute the Build flow and obtain the bundle.
2. Select a closed QEMU boot policy according to the resolved platform.
3. Add devices according to the drivers compiled into the kernel and the CLI options.
4. Launch QEMU with structured arguments and propagate the exit status.
5. After the unittest QEMU exits successfully, verify the `ALL_TESTS_PASSED` and
   `COVERAGE_PERSISTED` markers in this run's `qemu.log`. The latter is published only after the guest confirms that the coverage has been fully written, the file fsynced, and the
   filesystem synced. When a marker is missing or `TESTS_FAILED` is present, the profile from an old image is rejected.
6. After verification passes, extract `default.profraw` from the disk image, and then generate in order
   `default.profdata`, `coverage.txt`, `coverage.info`, and `coverage.xml`.

Coverage artifacts are located in `target/<rust-target>/<profile>/`. Old
artifacts are deleted before generation starts, and the failure of any step makes this run fail, so that CI does not consume stale reports.

### Doc

1. As in the build flow, obtain the configuration snapshot through a single `ConfigEngine` evaluation.
2. Generate the build configuration and the linker script from the snapshot, and obtain the
   Cargo feature set directly.
3. Invoke `cargo doc --workspace --no-deps` with the resolved kernel Rust target,
   without interpreting the kernel ABI on the host target and without parsing the generated Cargo TOML.
4. `--check-missing` rejects missing rustdoc, on top of retaining the broken-link/cfg checks.

### Hygiene Deps

1. Verify that the `cargo-shear` version in PATH matches the version pinned by XKMake;
   when the tool is missing or the version does not match, direct the user to run `make install-tools`.
2. Run the check in turn on the root workspace, `xtask`, `tee_apps`, and `uapps/hello`.
3. Pass `--locked` only to workspaces whose `Cargo.lock` is tracked by Git (the kernel root workspace),
   to prevent the hygiene check from implicitly rewriting the committed lockfile; the lockfiles of `xtask`,
   `tee_apps`, and `uapps/hello` are gitignored and have no committed lock,
   so `--locked` is not passed and cargo generates them as needed.
4. `--fix` only passes through the dependency-removal capability of cargo-shear; warnings keep their hint semantics, while
   an error or the failure of any workspace check terminates the command.

### Hygiene Header

1. Verify that the `licensure` version in PATH matches the version pinned by XKMake; when the tool is missing or
   the version does not match, direct the user to run `make install-tools`.
2. Obtain the tracked and the untracked-but-not-ignored Rust source files through Git, and filter out the paths that have been
   deleted in the working tree; non-Rust files do not enter the header check.
3. Pass the files to licensure in batches, to avoid hitting the process argument length limit as the repository grows.
4. In normal mode, run `--check`; `--fix` first runs `--in-place`, and then rechecks the final state with `--check`.
   The header template and comment format are defined by the repository-root `.licensure.yml`.

### Hygiene Lints

1. Run `cargo metadata --no-deps` in the root workspace and use the
   `workspace_members` resolved by Cargo as the source of truth; path dependencies inside the workspace directory are implicit members,
   so the `members` glob alone is not sufficient.
2. Parse each member manifest and require `lints.workspace = true`; members where it is missing or
   `false` are listed all at once and the check fails. There is no registered exemption list.
3. `make clippy`, `make dylint` and `make dylint_test` run
   this check before the Cargo checks, so the CI clippy stage rejects any crate that does not inherit `[workspace.lints]`.

The x86_64 bundle carries both `kernel.bzimg` and `kernel.uefi.img`. LinuxBoot is used by default,
and `--boot uefi` uses OVMF. The OVMF VARS template is copied to
`target/xkmake/runtime/<platform>/` on every run, so neither the firmware template nor the bundle is modified.

vsock uses capability probing: when the kernel enables virtio-socket and the user has not passed `--no-vsock`,
XKMake queries the device list of the current QEMU.
The device is added only if the corresponding `vhost-vsock-*` model
exists; if it is not supported, a warning is printed and startup continues.

The network forwarding port and the vsock CID are probed sequentially. `--hostfwd-port` (default 61005)
is the preferred host port forwarded to guest TCP/UDP 5555. If it is in use, XKMake searches `61005..=62005` in steps of 10
(61005, 61015, ..., 62005) for the first port that is free for both TCP and UDP;
when the probe range is exhausted, it exits with an error. Port probing treats only `AddrInUse` as
in use; any other bind error (such as a non-root user binding a port below 1024) is reported as an error directly, and the port is not silently
changed. `--vsock-cid` (default 103) is the preferred guest CID; if it is in use, XKMake searches `103..=203`
one by one in order for a free CID. CID probing is done by temporarily opening
`/dev/vhost-vsock` and performing `VHOST_VSOCK_SET_GUEST_CID`; the kernel uses
`EADDRINUSE` to indicate that the CID is already in use, and the probe fd is then closed to release the CID. If the device is missing or
the user lacks permission to probe, XKMake falls back to the preferred value so that a probe failure does not prevent the virtual machine from starting, while the kernel's `EINVAL`
(reserved CID) is reported as an error directly; `--vsock-cid` is validated during argument parsing to be at least 3 (the kernel
reserves 0/1/2). `--dry-run` performs no probing and builds
the command line directly from the preferred port and CID.

`--no-build` accepts only an existing bundle whose manifest, build information, embedded Build ID, artifacts and Cargo ELF
time all match. The reuse check recomputes the ELF loadable-image hash
to avoid consuming a modified or incomplete bundle. The `config` subcommand provides the Makefile tool targets with read-only queries for the architecture,
target, platform and bundle path, so the Makefile does not need to re-parse `.config`.

### Dylint

1. `make dylint` builds the lint library (`tools/lints`) with its pinned
   toolchain into `<target>/tools/lints`, then runs `cargo dylint
   --lib-path` on the kernel with the configuration's features, target,
   rustflags and environment; Dylint loads the prebuilt library and does
   not build it. `make dylint_test` runs the library's tests in the same
   target directory.
2. The library build runs with an empty `CARGO_ENCODED_RUSTFLAGS` and
   `CARGO_TARGET_DIR` set to that directory. Cargo in `tools/lints` still
   reads the repository `.cargo/config.toml` and the generated
   `.xconfig.toml` it includes, whose `[build] rustflags` change with the
   configuration (`--cfg k_plat_name=...`, `--cfg unittest`). The library
   uses none of them, but Dylint records it as a file dependency of every
   crate it checks, so relinking it for another configuration would
   re-check every crate. With the rustflags pinned, the library is rebuilt
   only when its sources, `Cargo.lock` or the toolchain change.
   `CARGO_TARGET_DIR` also reaches the library build that `dylint_testing`
   runs from the tests, which otherwise lands in `tools/lints/target`.
3. The cfgs the lints see (`k_plat_name`, `unittest`) belong to the
   kernel crates and still come from the configuration. Scans of different
   configurations share `<target>/dylint`; Cargo keeps separate artifacts
   for different rustflags. A crate is still re-checked when an input it
   shares between configurations changes, such as the generated
   `<target>/kbuild/<platform>/config.rs` that `kbuild_config` includes
   (it differs when, for example, lock statistics are on).

### Dylint Facts

`make dylint FACTS_OUT=<dir>` (`xkmake dylint --facts-out <dir>`) exports
code facts during the normal Dylint scan and packs them. The format and the
records are described in `tools/lints/README.md`.

1. Before anything else the run removes the previous artifact
   (`<dir>/status.json`, `manifest.json` and `crates/`), so that a run that
   fails while reading the kernel configuration leaves no status rather
   than a stale one. Once the configuration is read, it writes
   `status.json` (`running`); errors before the scan starts, such as a tool
   probe or the lint library build, are written as `failed`/`build`.
2. The scan process gets `XKERNEL_FACTS=1`, and Cargo gets
   `--message-format=json-render-diagnostics`: diagnostics are still
   rendered to stderr, and the JSON messages go to
   `<target>/dylint/facts-messages.jsonl`. The file is kept when the scan
   fails, so that the crates checked before the failure are packed.
3. With `workspace_members` from `cargo metadata --no-deps` as the member
   set, the metadata path of each unit is taken from the
   `compiler-artifact` messages (including `fresh` units), which gives the
   `<stem>.facts.jsonl` the lint writes next to it; build scripts are
   skipped.
4. Each file's header (schema, stem) and final `counts` line are
   validated; the file is zstd-compressed into `crates/` with its sha256
   and record count. Missing or invalid files are listed in `missing`.
5. `manifest.json` is written (commit, merge base, target, platform,
   profile, sha256 of `.config`, features, scan rustflags, toolchain, rustc
   version, exporter source digest and Dylint version, crate list and
   totals), and `status.json` last: `ready`, `partial` or `failed`, with
   reason `build` or `exporter`.

During an export the scan process also gets
`XKERNEL_FACTS_ROOT=<repository root>`: Cargo compiles crates.io and git
dependencies in their own package directories and passes relative paths, so
the exporter uses this root to write paths as repository-relative or as
`crates/<name>-<version>/…`. The variable is not registered as an
environment dependency and does not affect the cache.

Without `--facts-out` the run leaves `XKERNEL_FACTS` unchanged: a caller
that pins it to `1` (the whole CI dylint stage) keeps Cargo's cache in later
scans and only writes extra facts files.

`--facts-deps` (or `XKERNEL_FACTS_DEPS=1`) also exports dependency crates:
the scan sets XKMake itself as Cargo's `RUSTC_WRAPPER`
(`XKMAKE_FACTS_WRAPPER_TARGET` gives the kernel target). `main` recognizes
this environment variable before parsing the command line and, when the
first argument is a compiler (`rustc`, Cargo's `RUSTC` or Dylint's
driver), runs as the wrapper: workspace members (whose first argument is Dylint's
`RUSTC_WORKSPACE_WRAPPER`) and host crates are `exec`ed unchanged; a
dependency compiled for the kernel target is compiled by the Dylint driver
instead, with `--force-warn facts_export` appended and
`XKERNEL_FACTS_KIND=dep` set. The `--cap-lints allow` that Cargo passes for
dependencies still silences the other lints; only the export lint is forced
on. When packing, non-member units whose paths lie under the kernel target
directory are taken in as `dep` crates too; the manifest records
`deps: true`, and their crate entries carry the dependency's features.

## Design Decisions

- Cargo 负责源码增量编译，bundle 只缓存镜像加工结果。
- 普通 build 的复用属于可信本地缓存边界，依靠 manifest 最后提交的不变量、
  产物大小与 mtime 快速判断；`--no-build` 属于已有制品消费边界，额外执行
  完整 loadable-image SHA-256 校验。
- 每次 build 仍调用 Cargo 检查源码新鲜度；源码和配置未变化时 Cargo
  增量流程不重新编译，XKMake 也不重新生成 ELF metadata、BIN 或启动镜像。
  KFEAT_SYMTAB 的符号表自举在 blob 内容不变时不触发第二次链接。
- 构建来源元数据不进入 Cargo 编译环境；linker 预留固定 note，XKMake 在
  bundle 阶段原地写入，因此机器名和时间变化不会让 kernel crate 重编译。
- 自动构建时间表示 bundle 本次真正生成的 UTC 时间，不是缓存检查输入；
  `KBUILD_BUILD_TIME` 或 `SOURCE_DATE_EPOCH` 是显式覆盖值，值发生变化时会
  使已有 bundle 失效。
- 镜像元数据 ABI 由 `boot/kernel-image-metadata` 持有；XKMake 负责收集，
  entry 只负责校验后的展示。
- linker script 是配置派生的 host 编排产物，由 XKMake 在调用
  Cargo 前生成；`kernel-boot` 不再用 `build.rs` 生成它。
- `.config` 仍是内核产品配置的唯一事实来源。
- QEMU 参数使用 `Vec<OsString>` 语义逐项传递，不经过 shell 求值。
- `xkmake` 仅依赖 `xconfig::build_config` 门面，不依赖 CLI 实现模块。
- 没有 `.config` 时不猜测默认平台。
- bundle 是规范产物位置；仓库根目录的 `xkernel_<platform>.*` 是从有效
  bundle 刷新的兼容副本，不参与 bundle 有效性判断。
- LinuxBoot 镜像布局由 XKMake 的 Rust 代码生成，不依赖 Python 脚本。
- rustdoc feature 与内核构建使用同一份解析快照，不维护第二套配置读取器。
- 依赖分析使用固定版本的外部 cargo-shear，不在 XKMake 内复制 Cargo/Rust
  解析逻辑，也不把其大型依赖树链接进 XKMake。
- Rust 源文件 header 由固定版本的外部 licensure 按仓库配置检查和修复；
  XKMake 只负责确定文件集合、分批调用和传播失败状态。
- Code facts are exported by the lint library inside rustc; XKMake only
  selects the units of this build, validates and compresses their files and
  writes the manifest. Units are selected from Cargo's JSON messages rather
  than by scanning directories, so facts files left by other configurations
  in the same target directory are never mixed in. The export switch is an
  environment variable with the fixed value `1`, so that a changing value
  does not make Cargo re-check every crate.
- guest IP 和 gateway 当前属于构建输入，因为现有网络栈通过
  `option_env!` 编译它们；后续应迁移为内核命令行参数。

## Current Limitations

- AArch64, RISC-V 64 and LoongArch64 QEMU virt boot directly from the raw BIN.
- x86_64 LinuxBoot and UEFI are supported; other x86 platforms still need a separate boot strategy.
- unittest already generates an LLVM coverage report; richer aggregation of test results has not yet been migrated.
