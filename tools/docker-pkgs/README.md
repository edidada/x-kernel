# tools/docker-pkgs — 离线 Docker 预构建资产

本目录支撑 X-Kernel guest 内的离线 Docker 支持（见 `docs/docker-support.md`）。

**原则**：guest 运行时不做任何网络操作。docker 引擎（`dockerd`/`containerd`/`runc`
及 CLI 插件）的 arm64 deb 闭包与容器镜像，在**宿主侧有网环境一次性**准备并固化
校验和；之后再通过 `make dockerfs` 注入 `disk.img`。

## 目录结构

```text
tools/docker-pkgs/
├── README.md
├── build-deb-closure.sh   # 一次性生成 deb 闭包与镜像（需要网络）
├── manifest.schema.json   # manifest.json / SHA256SUMS 生成格式说明
├── manifest.json          # 生成产物（gitignored）：版本、sha256、来源记录
├── SHA256SUMS             # 生成产物（gitignored）：sha256 校验清单
└── aarch64/
    ├── debs/              # 生成产物（gitignored）：*.deb 闭包
    └── images/            # 生成产物（gitignored）：docker save *.tar
```

## 使用流程

```bash
# 1. 在有网机器上准备一次资产（arm64 宿主，或配好 qemu-user-static/binfmt 的 amd64
#    宿主；详见脚本头部说明）。产物落在本目录，之后可随制品库/CI 分发。
make docker-assets

# 2. 注入 disk.img（离线，无网络，无需宿主 root）
make rootfs       # 基础 rootfs（只在首次或想刷新时执行）
make dockerfs     # 等价 uapp 安装 docker-offline：注入 deb 闭包、镜像、配置与服务脚本
```

不运行 `docker-assets` 时 `make dockerfs` 会快速失败并给出指引，避免半注入状态。

## 生成内容与可重复性

`build-deb-closure.sh` 输出：

- `aarch64/debs/`：docker-ce、docker-ce-cli、containerd.io、docker-buildx-plugin、
  docker-compose-plugin 及其依赖的 arm64 deb 闭包（在干净的 Debian bookworm arm64
  容器内 `apt-get install --download-only` 生成，避免依赖漂移）；
- `aarch64/images/`：`hello-world`、`busybox`、`alpine` 的 arm64 `docker save` tar；
- `manifest.json`：版本、sha256、生成命令记录；
- `SHA256SUMS`：`sha256sum -c` 可直接校验的清单。

注入（`uapps/docker-offline` 的 prepare 阶段）先做 `sha256sum -c` 校验，失败即中止。
新增自定义镜像：有网环境 `docker save -o aarch64/images/<name>.tar <image>` 后重新
生成 SHA256SUMS 与 manifest，再 `make dockerfs`。

## 版本固定原则

- 只允许固定版本（随 deb 文件名天然固定），不追踪 latest；
- `manifest.json` 的 `commands` 字段记录生成命令，保证可审计复现；
- deb 闭包在干净镜像内生成，宿主软件源/内核差异不进入产物。