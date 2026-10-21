#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

# Exercise the pre-push staleness check without touching the network.
set -euo pipefail
HOOK="$(cd "$(dirname "$0")/.." && pwd)/pre-push"
FIXTURE_ROOT=$(mktemp -d)
export FIXTURE_ROOT
trap 'rm -rf "$FIXTURE_ROOT"' EXIT
mkdir "$FIXTURE_ROOT/bin"
cat > "$FIXTURE_ROOT/bin/git" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "$FIXTURE_ROOT/git-calls"
case "$1" in
rev-parse) exit 0 ;;
symbolic-ref)
    [[ -n "${BRANCH_NAME:-}" ]] && printf '%s\n' "$BRANCH_NAME"
    exit "${SYMBOLIC_STATUS:-0}" ;;
fetch) exit "${FETCH_STATUS:-0}" ;;
rev-list)
    [[ "${COMPARE_FAIL:-0}" = 1 ]] && exit 99
    printf '%s\n' "${BEHIND_COUNT:-0}" ;;
*) exit 99 ;;
esac
STUB
chmod +x "$FIXTURE_ROOT/bin/git"

PUSH_LINE='refs/heads/feat 1111111111111111111111111111111111111111 refs/heads/feat 2222222222222222222222222222222222222222'

# run_case NAME EXPECTED_STATUS EXPECTED_OUTPUT_SUBSTRING [VAR=value ...]
# An empty EXPECTED_OUTPUT_SUBSTRING asserts silent output. STDIN_TEXT, when
# set, replaces the default push line fed to the hook.
run_case() {
    local name=$1 expected_status=$2 expected_output=$3
    shift 3
    local status=0
    env PATH="$FIXTURE_ROOT/bin:$PATH" SKIP_REBASE=0 SKIP_ALL=0 \
        BRANCH_NAME=feat FETCH_STATUS=0 BEHIND_COUNT=0 COMPARE_FAIL=0 "$@" \
        bash "$HOOK" <<<"${STDIN_TEXT:-$PUSH_LINE}" \
        > "$FIXTURE_ROOT/output" 2>&1 || status=$?
    if [[ "$status" != "$expected_status" ]]; then
        printf 'FAIL: %s (exit %s, expected %s)\n' "$name" "$status" "$expected_status" >&2
        cat "$FIXTURE_ROOT/output" >&2
        exit 1
    fi
    if [[ -z "$expected_output" ]]; then
        if [[ -s "$FIXTURE_ROOT/output" ]]; then
            printf 'FAIL: %s (expected silent output)\n' "$name" >&2
            cat "$FIXTURE_ROOT/output" >&2
            exit 1
        fi
    elif ! grep -q "$expected_output" "$FIXTURE_ROOT/output"; then
        printf 'FAIL: %s (missing %q in output)\n' "$name" "$expected_output" >&2
        cat "$FIXTURE_ROOT/output" >&2
        exit 1
    fi
    printf 'PASS: %s\n' "$name"
}

run_case blocked_when_behind 1 'make rebase' BEHIND_COUNT=3
run_case blocked_output_names_the_base 1 'behind origin/main' BEHIND_COUNT=3
run_case pass_when_current 0 '' BEHIND_COUNT=0
run_case skip_rebase_pushes_anyway 0 '' SKIP_REBASE=1 BEHIND_COUNT=3
run_case skip_all_pushes_anyway 0 '' SKIP_ALL=1 BEHIND_COUNT=3
run_case offline_passes_with_warning 0 'cannot refresh' FETCH_STATUS=1 BEHIND_COUNT=3
run_case failed_count_passes_with_warning 0 'cannot compare' COMPARE_FAIL=1
run_case tag_refs_pass 0 '' STDIN_TEXT='refs/tags/v0 1111111111111111111111111111111111111111 refs/tags/v0 2222222222222222222222222222222222222222'
run_case multi_refspec_passes 0 '' STDIN_TEXT="$PUSH_LINE
refs/heads/next 3333333333333333333333333333333333333333 refs/heads/next 4444444444444444444444444444444444444444"
run_case branch_deletion_passes 0 '' STDIN_TEXT='refs/heads/feat 0000000000000000000000000000000000000000 refs/heads/feat 2222222222222222222222222222222222222222'
run_case other_branch_passes 0 '' STDIN_TEXT='refs/heads/other 1111111111111111111111111111111111111111 refs/heads/other 2222222222222222222222222222222222222222'
run_case detached_head_passes 0 '' SYMBOLIC_STATUS=1 BRANCH_NAME=
run_case stale_main_still_blocks_target_branch 1 'DIVERGED' STDIN_TEXT='refs/heads/feat 1111111111111111111111111111111111111111 refs/heads/feat 0000000000000000000000000000000000000000' BEHIND_COUNT=2
