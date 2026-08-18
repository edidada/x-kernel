#!/bin/bash
# Export corpus + parameters for entrypoints that reached >= MIN_COV on the WFUZZ platform.
#
# Usage:
#   ./export_passing_corpus.sh              # default: cov >= 20%, output fuzz-export/
#   ./export_passing_corpus.sh 15 /tmp/out  # cov >= 15%, custom output dir
#
# Deliverable layout:
#   <out>/
#     manifest.json          # summary: coverage, test id, file counts
#     wfuzz.json             # project / entrypoint definitions
#     fuzz_build.sh          # build helper
#     seed_corpus.sh         # how initial seeds were installed
#     generate_seeds.py
#     fuzz_targets/<ep>.rs   # harness for each exported entrypoint
#     <entrypoint>/
#       stat.json            # platform coverage proof
#       fuzzers-config.json  # runtime env (WFUZZ_TEST_ENTRYPOINT, dirs, ...)
#       corpus/              # input + output merged (replay with cargo-fuzz/wfuzz)
#       README.txt           # quick replay notes
set -euo pipefail

cd "$(dirname "$0")"
MIN_COV="${1:-20}"
OUT="${2:-fuzz-export}"
MIN_COV_FRAC="$(python3 -c "print(float('$MIN_COV')/100)")"
export MIN_COV_FRAC OUT_DIR="$OUT"
mkdir -p "$OUT"

python3 - <<'PY'
import json, glob, os, shutil, datetime

min_cov = float(os.environ["MIN_COV_FRAC"])
out = os.environ["OUT_DIR"]

# Latest wfuzz-test dir per entrypoint.
latest = {}
for d in glob.glob("wfuzz-test-*"):
    cfg = f"{d}/1/fuzzers-config.json"
    stat = f"{d}/1/stat.json"
    if not os.path.isfile(cfg) or not os.path.isfile(stat):
        continue
    ep = json.load(open(cfg))["env"]["WFUZZ_TEST_ENTRYPOINT"]
    tid = int(d.split("-")[-1])
    if ep not in latest or tid > latest[ep][0]:
        latest[ep] = (tid, d, json.load(open(stat)))

manifest = {
    "exported_at": datetime.datetime.now().isoformat(timespec="seconds"),
    "min_coverage_percent": min_cov * 100,
    "entrypoints": [],
}

for ep, (tid, test_dir, stat) in sorted(latest.items(), key=lambda x: -x[1][2].get("paths_total_coverage", 0)):
    cov = stat.get("paths_total_coverage", 0)
    if cov < min_cov:
        continue

    ep_dir = os.path.join(out, ep)
    corpus_dir = os.path.join(ep_dir, "corpus")
    os.makedirs(corpus_dir, exist_ok=True)

    for sub in ("input", "output"):
        src = os.path.join(test_dir, "1", sub)
        if os.path.isdir(src):
            for name in os.listdir(src):
                s = os.path.join(src, name)
                if os.path.isfile(s):
                    shutil.copy2(s, os.path.join(corpus_dir, name))

    shutil.copy2(os.path.join(test_dir, "1", "stat.json"), os.path.join(ep_dir, "stat.json"))
    shutil.copy2(os.path.join(test_dir, "1", "fuzzers-config.json"), os.path.join(ep_dir, "fuzzers-config.json"))
    wcov = os.path.join(test_dir, "1", "wcov.json")
    if os.path.isfile(wcov):
        shutil.copy2(wcov, os.path.join(ep_dir, "wcov.json"))

    n_corpus = len([f for f in os.listdir(corpus_dir) if os.path.isfile(os.path.join(corpus_dir, f))])
    with open(os.path.join(ep_dir, "README.txt"), "w") as f:
        f.write(f"entrypoint: {ep}\n")
        f.write(f"platform_test: wfuzz-test-{tid}\n")
        f.write(f"paths_total_coverage: {cov*100:.2f}%\n")
        f.write(f"execs_done: {stat.get('execs_done', 0)}\n")
        f.write(f"corpus_files: {n_corpus}\n")
        f.write("\nReplay locally (container):\n")
        f.write(f"  cd /code/fuzz\n")
        f.write(f"  ./fuzz_build.sh\n")
        f.write(f"  cargo fuzz run {ep} corpus-export/{ep}/corpus -- -runs=0\n")
        f.write("\nPlatform replay:\n")
        f.write(f"  WFUZZ_TEST_ENTRYPOINT={ep} wfuzz fuzz\n")
        f.write(f"  # copy corpus/* into wfuzz-test-*/1/input/\n")

    harness = f"fuzz_targets/{ep}.rs"
    if os.path.isfile(harness):
        os.makedirs(os.path.join(out, "fuzz_targets"), exist_ok=True)
        shutil.copy2(harness, os.path.join(out, "fuzz_targets", f"{ep}.rs"))

    manifest["entrypoints"].append({
        "name": ep,
        "platform_test_id": tid,
        "paths_total_coverage_percent": round(cov * 100, 2),
        "execs_done": stat.get("execs_done", 0),
        "paths_total": stat.get("paths_total", 0),
        "corpus_files": n_corpus,
        "test_dir": test_dir,
    })

with open(os.path.join(out, "manifest.json"), "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")

print(f"exported {len(manifest['entrypoints'])} entrypoints (>={min_cov*100:.0f}%) -> {out}")
for e in manifest["entrypoints"]:
    print(f"  {e['name']}: {e['paths_total_coverage_percent']}% test={e['platform_test_id']} corpus={e['corpus_files']}")
PY

for f in wfuzz.json fuzz_build.sh seed_corpus.sh generate_seeds.py extract_entrypoints_list.py; do
  [ -f "$f" ] && cp -f "$f" "$OUT/"
done

echo ""
echo "Archive for colleagues:"
echo "  tar -czf x-kernel-fuzz-export-$(date +%Y%m%d).tar.gz -C $(dirname "$OUT") $(basename "$OUT")"
echo "Host path: $(cd "$OUT" && pwd)"
