---
title: "Getting Help"
description: "Report issues and join the community"
---

# Getting Help

Use the [X-kernel community](https://x-kernel.zulipchat.com) for usage questions and design discussions. Report reproducible defects or clearly described improvements through [Gitee Issues](https://gitee.com/openkylin/x-kernel/issues).

## Before asking

1. Check [Quick Start](quick-start.en.md), including the selected platform and command order.
2. Search existing issues and community discussions for the same symptom.
3. Reduce the reproduction steps and identify the first failing command. A final `make` error may only summarize an earlier failure.

## Provide reproducible information

Include your host system, repository revision, platform configuration, toolchain versions, rootfs source, and exact steps. Start by collecting:

```bash
git rev-parse HEAD
git status --short
rustc --version
qemu-system-aarch64 --version
```

Substitute the appropriate QEMU command for another architecture. Name the defconfig you selected and describe subsequent menuconfig or manual changes.

In Bash, capture the build log while preserving a failed build's exit status:

```bash
set -o pipefail
make build 2>&1 | tee build.log
```

For boot problems, include serial output from startup to the failure, not just a screenshot of the final error. Check logs for local paths, credentials, or other private information before sharing.

## Issue template

```text
Summary:
Expected behavior:
Actual behavior:
Repository revision:
Host OS / CPU architecture:
Platform defconfig and additional changes:
Rust / QEMU / cross-compiler versions:
Rootfs source:
Reproduction commands:
First error and complete logs:
Local source changes:
```

For feature requests, explain the use case, current limitation, and desired behavior. Questions without reproduction steps can start in the community; you do not need to understand the whole kernel before asking.

## Security issues

If a problem may involve an exploitable vulnerability or sensitive information, consult the repository's current disclosure instructions first. If none are provided, ask maintainers for a private channel before sharing sensitive details. This guide does not invent a security contact address or response commitment.

## Contributing

Read the repository's current contribution instructions and coding conventions. Connect the reported problem to relevant source code. When proposing changes, explain the motivation, validation, and affected behavior. Crate-local documentation remains the reference for internal design and interface details.
