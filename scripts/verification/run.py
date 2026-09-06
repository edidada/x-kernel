#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run a crate-local verification package without kernel Cargo configuration."""

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile

from reports import check_reports, check_sources, digest

ROOT = Path(__file__).resolve().parents[2]


def execute(command, *, cwd, env, log=None):
    if log is None:
        subprocess.run(command, cwd=cwd, env=env, check=True)
        return
    with log.open("w") as output:
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True)
        for line in process.stdout:
            print(line, end="", flush=True)
            output.write(line)
        if process.wait():
            raise subprocess.CalledProcessError(process.returncode, command)


def snapshot(config_path, manifests):
    # Include all local dependency sources, verification tools/config and locks.
    paths = {ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "rust-toolchain.toml"}
    for manifest in manifests:
        paths.add(manifest)
        paths.update(manifest.parent.rglob("*.rs"))
        lock = manifest.parent / "Cargo.lock"
        if lock.exists():
            paths.add(lock)
    paths.update((ROOT / "scripts/verification").glob("*.py"))
    paths.add(config_path)
    return {str(path): digest(path) for path in sorted(paths) if path.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("crate", type=Path, help="production crate directory, relative to repository")
    parser.add_argument("-j", type=int, default=4)
    parser.add_argument("--harness", action="append", default=[], help="diagnostic subset, not acceptance")
    args = parser.parse_args()
    if args.j < 1:
        parser.error("jobs must be positive")
    crate = (ROOT / args.crate).resolve()
    crate.relative_to(ROOT)
    verification = crate / "verification"
    config_path = verification / "config.json"
    config = json.loads(config_path.read_text())
    if config["schema"] != 2 or ROOT / config["production"] != crate:
        raise ValueError("unsupported or mismatched verification configuration")
    if (platform.system(), platform.machine()) != ("Linux", "x86_64"):
        raise ValueError("reviewed verification platform requires Linux x86_64")
    if not args.harness:
        check_sources(ROOT, config)
    kani = shutil.which("cargo-kani")
    cvc5 = shutil.which("cvc5")
    if not kani or not cvc5:
        raise ValueError("install the Kani/CVC5 versions listed in verification/config.json")
    kani_version = subprocess.check_output([kani, "--version"], text=True).strip()
    cvc5_version = subprocess.check_output([cvc5, "--version"], text=True).splitlines()[0]
    if kani_version != config["tools"]["kani"] or cvc5_version != config["tools"]["cvc5"]:
        raise ValueError(f"unreviewed tool versions: {kani_version}; {cvc5_version}")
    artifact_root = ROOT / "target/kani" / crate.name
    artifact_root.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="run.", dir=artifact_root))
    print(f"Verification artifacts: {run}", flush=True)
    with tempfile.TemporaryDirectory(prefix="kernel-verification-") as temporary:
        work = Path(temporary)
        env = dict(os.environ)
        # Cargo searches config from cwd; run outside the kernel workspace.
        env.update(CARGO_BUILD_TARGET="x86_64-unknown-linux-gnu", RUSTFLAGS="--check-cfg=cfg(unittest)")
        env.pop("CARGO_ENCODED_RUSTFLAGS", None)
        env.pop("RUSTC_WRAPPER", None)
        env.pop("RUSTC_WORKSPACE_WRAPPER", None)
        env["RUSTUP_TOOLCHAIN"] = config["tools"]["host_toolchain"]
        wrapper = work / "cvc5"
        env["VERIFICATION_CVC5"] = cvc5
        wrapper.write_text('#!/bin/sh\nexec "$VERIFICATION_CVC5" --solve-bv-as-int=sum "$@"\n')
        wrapper.chmod(0o755)
        env["PATH"] = str(work) + os.pathsep + env["PATH"]
        manifest = verification / "Cargo.toml"
        metadata = json.loads(subprocess.check_output([
            "cargo", "metadata", "--locked", "--format-version", "1", "--manifest-path", str(manifest)
        ], cwd=work, env=env, text=True))
        manifests = [Path(p["manifest_path"]) for p in metadata["packages"] if p["source"] is None]
        before = snapshot(config_path, manifests)
        (run / "inputs.json").write_text(json.dumps(before, indent=2) + "\n")
        if not args.harness:
            execute([sys.executable, "-m", "unittest", "discover", "-s", str(ROOT / "scripts/verification"),
                     "-p", "test_*.py"], cwd=work, env=env, log=run / "tool-tests.log")
            rlib = work / ("lib" + config["library"] + ".rlib")
            execute(["rustc", "--edition=2024", "--crate-name", config["library"], "--crate-type", "rlib",
                     "--check-cfg=cfg(unittest)", str(crate / "src/lib.rs"), "-o", str(rlib)], cwd=work, env=env)
            for index, test in enumerate(config.get("doctests", [])):
                execute(["rustdoc", "--edition=2024", "--test", "--crate-name", config["library"],
                         "--check-cfg=cfg(unittest)", "--extern", f'{config["library"]}={rlib}',
                         str(verification / test)], cwd=work, env=env, log=run / f"doctest-{index}.log")
            for index, test in enumerate(config.get("host_tests", [])):
                binary = work / f"test-{index}"
                execute(["rustc", "--edition=2024", "--test", str(verification / test), "--extern",
                         f'{config["library"]}={rlib}', "-o", str(binary)], cwd=work, env=env)
                execute([str(binary)], cwd=work, env=env, log=run / f"host-test-{index}.log")
        env["CARGO_NET_OFFLINE"] = "true"
        command = [kani, "--manifest-path", str(manifest), "--target-dir", str(run),
                   "-j", str(args.j), "--harness-timeout", config["timeout"],
                   "--output-format", "terse", "--output-into-files", "-Z", "unstable-options"]
        for harness in args.harness:
            command.extend(["--harness", harness])
        if config.get("loop_bounds"):
            command.extend(["--cbmc-args", "--unwindset", config["loop_bounds"]])
        (run / "environment.json").write_text(json.dumps({
            "kani": kani_version, "cvc5": cvc5_version, "command": command,
            "host_toolchain": env["RUSTUP_TOOLCHAIN"], "RUSTFLAGS": env["RUSTFLAGS"],
        }, indent=2) + "\n")
        execute(command, cwd=work, env=env, log=run / "verification.log")
        if snapshot(config_path, manifests) != before:
            raise ValueError("verification inputs changed during this run")
        # Cargo Kani puts reports below its target-specific output directory.
        directories = list(run.rglob("result_output_dir"))
        if len(directories) != 1:
            raise ValueError("missing or ambiguous Kani report directory")
        if directories[0] != run / "result_output_dir":
            shutil.copytree(directories[0], run / "result_output_dir")
        result = check_reports(run, config, filtered=bool(args.harness))
        result["full_acceptance"] = not args.harness
        (run / "acceptance.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
