#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
#
# One-shot board bring-up loop for the rk3588 test rig (172.30.193.53):
#
#   1. build + deploy the kernel into the rig's TFTP root
#      (scripts/deploy-rk3588.sh --build, skipped with SKIP_BUILD=1)
#   2. run one rkt cycle on the rig: serial-relay power cycle → U-Boot →
#      tftpboot → boot → console capture, verdict from the profile's
#      ready/fail regex; a hanging boot ends via --until-quiet
#   3. fetch the console log back to /tmp for local analysis
#
# Usage:
#   scripts/board-run.sh <tag> [extra rkt run args...]
#   SKIP_BUILD=1 scripts/board-run.sh pcie-dbg        # reuse the pushed image
#
# The console log lands at /tmp/board-<tag>.log.

set -euo pipefail

RIG=${RIG:-kylin@172.30.193.53}
TAG=${1:?usage: board-run.sh <tag> [extra rkt run args...]}
shift || true
REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

echo "== [1/3] deploy kernel to the TFTP root"
if [[ ${SKIP_BUILD:-0} != 1 ]]; then
    "$REPO_ROOT/scripts/deploy-rk3588.sh" --seed-config --build
else
    echo "SKIP_BUILD=1 — pushing the existing image"
    "$REPO_ROOT/scripts/deploy-rk3588.sh"
fi

echo "== [2/3] rkt boot cycle (tag: $TAG)"
# --until-quiet 30: a hanging boot (e.g. the DBI bus stall) ends the cycle
# after 30s of console silence instead of blocking for the full 240s boot
# timeout; the console log still covers everything up to the stall.
# --no-preflash: the kernel is built HERE and pushed by step 1; the rig has
# no rust toolchain, so the runner's remote "make build" preflash must not run.
ssh -o BatchMode=yes "$RIG" "cd ~/rkt && ./rkt run --tag '$TAG' --until-quiet 30 --no-preflash $*"

echo "== [3/3] fetch the console log"
run_id=$(ssh -o BatchMode=yes "$RIG" "ls -t ~/rkt/logs/ | grep '$TAG' | head -1")
log="board-$TAG.log"
scp -q "$RIG:~/rkt/logs/$run_id/console.log" "/tmp/$log"
echo "console log: /tmp/$log (run $run_id)"
