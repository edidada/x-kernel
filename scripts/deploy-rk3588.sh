#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.
#
# Push the RK3588 kernel image to the board's programming host.
#
# Everything is configurable, in this order of precedence:
#   1. command-line flags              (--host/--user/--dir/--artifact/...)
#   2. environment variables           (DEPLOY_HOST, DEPLOY_USER, DEPLOY_DIR, ...)
#   3. scripts/deploy-rk3588.env       (local, gitignored, "KEY=value" lines)
#   4. the defaults below
#
# Typical use:
#   scripts/deploy-rk3588.sh                 # push the current build
#   scripts/deploy-rk3588.sh --build         # build first, then push
#   scripts/deploy-rk3588.sh --with-dtb orangepi-5-plus-m.dtb
#   scripts/deploy-rk3588.sh --dry-run
#
# The artifact must exist; a `--build` run skips work when nothing changed
# (make decides that), so running it after every edit is cheap.

set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

# ---------------------------------------------------------------- defaults --
DEPLOY_HOST=${DEPLOY_HOST:-172.30.193.53}
DEPLOY_USER=${DEPLOY_USER:-kylin}
DEPLOY_DIR=${DEPLOY_DIR:-/media/kylin/USR-DATA/xh/work/rk3588-adapt/new-framework-test/x-kernel}
DEPLOY_ARTIFACT=${DEPLOY_ARTIFACT:-$repo_root/xkernel_aarch64-rk3588.bin}
DEPLOY_SSH_OPTS=${DEPLOY_SSH_OPTS:--o BatchMode=yes -o ConnectTimeout=10}
DEPLOY_KEEP_PREVIOUS=${DEPLOY_KEEP_PREVIOUS:-1}

# Local overrides (never committed): one KEY=value per line.
env_file=${DEPLOY_ENV_FILE:-$repo_root/scripts/deploy-rk3588.env}
if [[ -f $env_file ]]; then
    # shellcheck disable=SC1090
    source "$env_file"
fi

build=0
dry_run=0
seed_config=0
with_dtb=""

usage() {
    cat <<'EOF'
Usage: scripts/deploy-rk3588.sh [options]

  --build              run `make build` before deploying
  --seed-config        copy platforms/kplat-aarch64/rk3588_defconfig to .config
                       and run `make defconfig` before building (discards local
                       .config edits)
  --with-dtb FILE      also deploy a device-tree blob (repeatable, comma separated)
  --host HOST          target host              (DEPLOY_HOST, default 172.30.193.53)
  --user USER          ssh user                 (DEPLOY_USER, default kylin)
  --dir DIR            remote directory         (DEPLOY_DIR)
  --artifact FILE      local image to deploy    (DEPLOY_ARTIFACT)
  --ssh-opts OPTS      extra ssh options        (DEPLOY_SSH_OPTS)
  --no-keep-previous   do not keep the previous remote file as .prev
  --env-file FILE      local config file        (default scripts/deploy-rk3588.env)
  --dry-run            print what would happen
  -h, --help           this text
EOF
}

dtb_files=()
while [[ $# -gt 0 ]]; do
    case $1 in
        --build) build=1 ;;
        --seed-config) seed_config=1; build=1 ;;
        --with-dtb)
            IFS=, read -r -a _extra <<< "${2:?--with-dtb needs a file}"
            dtb_files+=("${_extra[@]}")
            shift
            ;;
        --host) DEPLOY_HOST=${2:?--host needs a value}; shift ;;
        --user) DEPLOY_USER=${2:?--user needs a value}; shift ;;
        --dir) DEPLOY_DIR=${2:?--dir needs a value}; shift ;;
        --artifact) DEPLOY_ARTIFACT=${2:?--artifact needs a value}; shift ;;
        --ssh-opts) DEPLOY_SSH_OPTS=${2:?--ssh-opts needs a value}; shift ;;
        --no-keep-previous) DEPLOY_KEEP_PREVIOUS=0 ;;
        --env-file) env_file=${2:?--env-file needs a value}; shift ;;
        --dry-run) dry_run=1 ;;
        -h | --help) usage; exit 0 ;;
        *)
            echo "deploy-rk3588: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

run() {
    if ((dry_run)); then
        printf '  [dry-run] %s\n' "$*"
    else
        printf '  %s\n' "$*"
        "$@"
    fi
}

say() { printf '== %s\n' "$*"; }
die() { printf 'deploy-rk3588: %s\n' "$*" >&2; exit 1; }

# ------------------------------------------------------------------- build --
cd "$repo_root"

if ((seed_config)); then
    say "seeding .config from platforms/kplat-aarch64/rk3588_defconfig"
    run cp platforms/kplat-aarch64/rk3588_defconfig .config
    run make defconfig
fi

if ((build)); then
    [[ -f .config ]] || die ".config is missing; run with --seed-config or 'cp platforms/kplat-aarch64/rk3588_defconfig .config && make defconfig'"
    say "building"
    run make build
fi

# ---------------------------------------------------------------- validate --
[[ -f $DEPLOY_ARTIFACT ]] || die "artifact not found: $DEPLOY_ARTIFACT (run --build, or pass --artifact)"

bundle=$repo_root/target/xkmake/kplat-aarch64/release/bundle.toml
if [[ -f $bundle ]] && ! grep -q 'platform = "kplat-aarch64"' "$bundle"; then
    die "the last build is not the aarch64 platform bundle ($bundle)"
fi

target=$DEPLOY_USER@$DEPLOY_HOST
remote=$DEPLOY_DIR
say "artifact  $DEPLOY_ARTIFACT ($(stat -c %s "$DEPLOY_ARTIFACT") bytes)"
say "target    $target:$remote"

# Non-interactive ssh/scp. The target's shell banner or motd can land on
# stdout, so anything whose output is parsed below filters for its own shape
# instead of trusting "first line".
# shellcheck disable=SC2086
ssh_t() { ssh $DEPLOY_SSH_OPTS "$target" "$@"; }
# shellcheck disable=SC2086
ssh_quiet() { ssh $DEPLOY_SSH_OPTS "$target" "$@" >/dev/null 2>&1; }
# shellcheck disable=SC2086
scp_t() { scp -q $DEPLOY_SSH_OPTS "$@"; }

if ((dry_run)); then
    run ssh_t "mkdir -p '$remote'"
else
    ssh_quiet "mkdir -p '$remote'" || die "cannot reach $target:$remote"
fi

# --------------------------------------------------------------- transfer --
push() {
    local src=$1
    local base
    base=$(basename -- "$src")
    # Remote name: keep the caller's file name.
    if ((DEPLOY_KEEP_PREVIOUS)) && [[ $dry_run -eq 0 ]] && ssh_quiet "test -f '$remote/$base'"; then
        run ssh_quiet "cp -f '$remote/$base' '$remote/$base.prev'"
    fi
    if ((dry_run)); then
        run scp_t "$src" "$target:$remote/$base"
        return 0
    fi
    run scp_t "$src" "$target:$remote/$base"
    local local_sum remote_sum remote_out
    local_sum=$(sha256sum "$src" | cut -d' ' -f1)
    # Only the hex digest is taken from the remote side (a login banner must
    # not turn into a false mismatch), and its stderr is dropped because boards
    # and desktops like to print a motd there on every ssh call.
    # shellcheck disable=SC2086
    remote_out=$(ssh $DEPLOY_SSH_OPTS "$target" "sha256sum '$remote/$base'" 2>/dev/null) ||
        die "cannot verify $base on $target"
    remote_sum=$(printf '%s\n' "$remote_out" | grep -oE '[0-9a-f]{64}' | head -n1)
    if [[ $local_sum != "$remote_sum" ]]; then
        die "checksum mismatch for $base (local $local_sum, remote $remote_sum)"
    fi
    printf '  %s ok (%s)\n' "$base" "${local_sum:0:12}"
}

say "deploying"
push "$DEPLOY_ARTIFACT"
for dtb in "${dtb_files[@]}"; do
    [[ -n $dtb ]] || continue
    [[ -f $dtb ]] || die "device-tree blob not found: $dtb"
    push "$dtb"
done

if ((dry_run)); then
    say "dry run complete"
else
    say "done: $target:$remote"
fi