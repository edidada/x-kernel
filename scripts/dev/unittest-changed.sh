#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
#
# Map local changes to the kernel crates that own them and run only those
# crates' unit tests under QEMU:
#
#     make unittest-changed                     # uncommitted changes
#     make unittest-changed UNITTEST_BASE=origin/master
#     make unittest-changed UNITTEST_LIST=y     # print crates, do not run
#
# Crates without kernel unit tests are skipped, so docs/build-only changes
# do not accidentally start a QEMU run. Full unfiltered unit-test runs are
# the CI job's responsibility, not a local routine.

set -euo pipefail

usage() {
    echo "usage: $0 [--base <git-ref>] [--list]" >&2
    echo "  --base <git-ref>  include committed changes up to HEAD relative to <git-ref>" >&2
    echo "  --list            only print the affected crates, do not run QEMU" >&2
}

base=""
list_only=0
while [ $# -gt 0 ]; do
    case "$1" in
        --base)
            [ $# -ge 2 ] || { echo "error: --base needs a git ref" >&2; exit 2; }
            base="$2"
            shift 2
            ;;
        --list)
            list_only=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

changed="$(mktemp)"
crate_map="$(mktemp)"
selected="$(mktemp)"
skipped="$(mktemp)"
trap 'rm -f "$changed" "$crate_map" "$selected" "$skipped"' EXIT
if [ -n "$base" ]; then
    merge_base="$(git merge-base "$base" HEAD)"
    git diff --name-only "$merge_base" >>"$changed"
fi
# Uncommitted edits and untracked files are always included so an in-progress
# worktree is covered even when a committed range is requested.
git diff --name-only HEAD >>"$changed"
git ls-files --others --exclude-standard >>"$changed"
sort -u "$changed" -o "$changed"

if [ ! -s "$changed" ]; then
    echo "unittest-changed: no local changes detected"
    exit 0
fi

# Walk up from each changed file to the nearest owning crate (a directory
# with a Cargo.toml that declares [package]). Workspace-only manifests such
# as the repository root do not stop the walk. Host tool workspaces excluded
# from the kernel build (xtask, lints, user apps, TEE apps, fuzzing) never
# own kernel unit tests, so they are skipped before detection.
# Plain tab-separated files replace associative arrays so the helper also
# runs on hosts with the macOS system bash 3.2.
while IFS= read -r file; do
    [ -n "$file" ] || continue
    dir="$(dirname -- "$file")"
    case "$dir" in
        xtask/*|tools/*|uapps/*|tee_apps/*|fuzz/*|third_party/*) continue ;;
    esac
    while [ "$dir" != "." ]; do
        if [ -f "$dir/Cargo.toml" ] && grep -q '^\[package\]' "$dir/Cargo.toml"; then
            # UNITTEST_CRATE filters by the Rust crate identifier (the first
            # segment of module_path!()), where hyphens become underscores:
            # ktimer-core registers as ktimer_core.
            name="$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$dir/Cargo.toml" | head -1 | tr '-' '_')"
            if [ -n "$name" ]; then
                printf '%s\t%s\n' "$name" "$dir" >>"$crate_map"
            fi
            break
        fi
        dir="$(dirname -- "$dir")"
    done
done <"$changed"
sort -u "$crate_map" -o "$crate_map"

if [ ! -s "$crate_map" ]; then
    echo "unittest-changed: no workspace crates among the changed files"
    exit 0
fi

# Keep only crates that actually own kernel unit tests: a ut-tests/ directory
# or a registration through unittest's test macros. Changes to crates without
# tests (docs, host tools, layout-only edits) must not start a QEMU run.
while IFS=$'\t' read -r name dir; do
    [ -n "$name" ] || continue
    if [ -d "$dir/ut-tests" ] \
        || grep -rq --include='*.rs' \
            -e 'unittest::path_mod_test' \
            -e 'unittest::mod_test' \
            -e 'unittest::def_test' \
            -e '#\[def_test' \
            "$dir"; then
        printf '%s\n' "$name" >>"$selected"
    else
        printf '%s\n' "$name" >>"$skipped"
    fi
done <"$crate_map"
sort -u "$selected" -o "$selected"
sort -u "$skipped" -o "$skipped"

if [ -s "$selected" ]; then
    echo 'unittest-changed: crates with unit tests:'
    sed 's/^/  /' "$selected"
fi
if [ -s "$skipped" ]; then
    echo 'unittest-changed: changed crates without kernel unit tests (skipped):'
    sed 's/^/  /' "$skipped"
fi
if [ ! -s "$selected" ]; then
    echo "unittest-changed: no changed crate owns kernel unit tests; nothing to run"
    exit 0
fi

crate_filter="$(paste -sd, "$selected")"
if [ "$list_only" -eq 1 ]; then
    echo "UNITTEST_CRATE=$crate_filter"
    exit 0
fi

echo "unittest-changed: running: make unittest UNITTEST_CRATE=$crate_filter"
# Deliberately not exec: the EXIT trap must still clean up the temp files,
# and with `set -e` the last command's status propagates either way.
make unittest "UNITTEST_CRATE=$crate_filter"
