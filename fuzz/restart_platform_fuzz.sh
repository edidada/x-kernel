#!/bin/bash
# Stop platform fuzz jobs, install corpus seeds, rebuild, and restart all entrypoints.
set -euo pipefail

cd "$(dirname "$0")"
export PATH="${HOME}/.wfuzz/bin:${HOME}/.cargo/bin:${PATH}"
export RUSTUP_TOOLCHAIN=nightly

pkill -f wfuzz-fuzz-code-rust 2>/dev/null || true
pkill -f 'wfuzz-fuzz-targets/x86_64-unknown-linux-gnu/release/' 2>/dev/null || true
sleep 2

./seed_corpus.sh
./fuzz_build.sh >/tmp/wfuzz-build.log 2>&1
tail -5 /tmp/wfuzz-build.log

mkdir -p platform-runs
mapfile -t entrypoints < entrypoints_list.txt

declare -A test_dir_for

for ep in "${entrypoints[@]}"; do
  log="platform-runs/${ep}.log"
  : > "$log"
  nohup env WFUZZ_TEST_ENTRYPOINT="$ep" wfuzz fuzz >>"$log" 2>&1 &
  echo "started $ep pid=$!"
  sleep 2

  latest=""
  for _ in $(seq 1 30); do
    while IFS= read -r dir; do
      cfg="$dir/1/fuzzers-config.json"
      [ -f "$cfg" ] || continue
      found=$(python3 - <<PY "$cfg" "$ep"
import json, sys
cfg, ep = sys.argv[1], sys.argv[2]
print(json.load(open(cfg))["env"]["WFUZZ_TEST_ENTRYPOINT"] == ep)
PY
)
      if [ "$found" = True ]; then
        latest="$dir"
        break 2
      fi
    done < <(ls -td wfuzz-test-* 2>/dev/null | head -20)
    sleep 1
  done

  if [ -z "$latest" ]; then
    echo "warn: could not locate wfuzz-test dir for $ep" >&2
    continue
  fi

  test_dir_for[$ep]="$latest"
  mkdir -p "$latest/1/input"
  if [ -d "corpus/$ep" ]; then
    cp -f corpus/"$ep"/* "$latest/1/input/" 2>/dev/null || true
  fi
  echo "  -> $latest input=$(find "$latest/1/input" -type f | wc -l)"
done

echo ""
echo "restarted ${#entrypoints[@]} entrypoints with corpus seeds"
