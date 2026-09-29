# CI builder image

Jenkins 的所有阶段都在这个镜像里运行。镜像需要满足的运行契约见
[docs/jenkins-pipeline.md](../../../docs/jenkins-pipeline.md)。

## 目录与版本

```text
scripts/ci/docker/
├── build.sh                  # 构建一个版本
└── <os>_<release>/           # 一个基础系统一个目录，同时作为构建上下文
    ├── Dockerfile_v<X.Y>     # 一个版本一个文件
    └── files/                # 构建时 COPY 进镜像的文件
```

- 镜像标签取自文件名里的版本号：`<os>_<release>/Dockerfile_v<X.Y>` 对应
  `<repo>:v<X.Y>`。版本号在所有系统目录之间唯一。
- **完整版本**从按 digest 固定的基础镜像开始构建全部内容。基础镜像、工具和
  需要固定的软件包版本都写在 Dockerfile 开头的 `ARG` 里，这是版本号的唯一出处。
- 固定到精确版本号的 apt 包，在上游撤下旧版本后会导致构建失败，这时改 `ARG`
  并发布新版本。未固定的 apt 包跟随发行版的安全更新。
- **增量版本**新建一个 `Dockerfile_v<X.Y>`，开头写：

  ```dockerfile
  ARG BASE_IMAGE=<上一版镜像>@sha256:<digest>
  FROM ${BASE_IMAGE}
  ```

  文件里只写这一版的改动，构建时用 `--base <上一版镜像>@sha256:...` 指定上一版。
- 更换基础系统，或增量层积累太多时，新建一个完整版本（换系统时同时新建目录），
  主版本号加一。
- 已发布版本的文件不再修改，要改就发一个新版本。

## 构建与检查

构建需要 `docker buildx`。在 x86_64 主机上构建 arm64 镜像，还需要 QEMU
binfmt（Arch Linux 上是 `docker-buildx`、`qemu-user-static` 和
`qemu-user-static-binfmt`）。先创建一个支持多平台的构建器：

```sh
docker buildx create --name xkernel --driver docker-container --use
```

本地构建时每次只构建一个平台，结果加载到本机。两个平台用不同的 repo 名，
否则后构建的会覆盖先构建的标签：

```sh
DOCKERFILE=scripts/ci/docker/<os>_<release>/Dockerfile_v<X.Y>
scripts/ci/docker/build.sh "$DOCKERFILE" --repo xk-local-amd64
scripts/ci/docker/build.sh "$DOCKERFILE" --repo xk-local-arm64 --platform linux/arm64
```

在 QEMU 模拟下构建 arm64 比原生慢得多。buildx 构建器会保留缓存，之后的构建
只重做改动过的阶段。

修改 Dockerfile 或脚本后，运行：

```sh
docker buildx build --check -f "$DOCKERFILE" "$(dirname "$DOCKERFILE")"
shellcheck scripts/ci/docker/*.sh
```

## 发布

1. 两个平台的本地镜像都满足运行契约，并能完成内核构建。
2. 推送 multi-arch 镜像，记下输出的 digest：

   ```sh
   scripts/ci/docker/build.sh "$DOCKERFILE" --repo <registry>/x-kernel-builder --push
   ```

3. 把镜像引用换成 `<repo>:<tag>@sha256:<digest>`：
   - 本仓库 `Jenkinsfile` 顶部的 `agent { docker { image ... } }`；
   - starry-test-harness 仓库 `Jenkinsfile` 中的 `dailyBuilderImage()`。

   PR 的 CI 读取的是 PR 合并引用里的 `Jenkinsfile`，所以换镜像的 PR 自己的 CI
   就会用新镜像运行。
