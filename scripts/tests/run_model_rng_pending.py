#!/usr/bin/env python3
"""Host-only pending RNG experiment, using the standard make/coverage path.

Requires a prepared aarch64 QEMU MMIO config, RNG driver enabled, virtio
entropy consumer disabled, and a private regular disk.img in this worktree.
Example: python3 scripts/tests/run_model_rng_pending.py --log-dir /tmp/pending-01

The EGD gate never writes a response. Socket receipt and stdout receipt have
independent buffering: their host observation order is NOT guest chronology.
Positive requests plus the sole-reader configuration and ordered guest markers
establish a pending-read experiment, not IRQ callback/DMA-tail quiescence proof.
The watchdog covers make, QEMU and coverage processing. On timeout/interruption
it sends TERM then KILL to the private make process group, including descendants;
it never kills by executable name or touches another run's process group.
"""

import argparse
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time


TEST_NAME = "real_model_mmio_rng_pending_unbind_releases_reader"
BLOCKED = "MODEL_MMIO_PENDING: BLOCKED"
CLOSED = "MODEL_MMIO_PENDING: CLOSED"
PASSED = "=== UNITTEST_STATUS: ALL_TESTS_PASSED ==="
COMMAND = [
    "make", "unittest", "UNITTEST_CRATE=kdriver",
    "XKMAKE_ARGS=--no-net --no-vsock",
    "QEMU_ARGS=-global virtio-mmio.force-legacy=false",
]


def rewrite_argv(arguments, socket_path):
    """Replace the sole RNG backend, retaining the existing device and id."""
    if arguments.count("-kernel") != 1:
        raise ValueError("expected exactly one -kernel invocation")
    objects = []
    devices = []
    for index, argument in enumerate(arguments):
        if argument in ("-object", "-device", "-kernel"):
            if index + 1 == len(arguments) or arguments[index + 1].startswith("-"):
                raise ValueError("missing QEMU option value")
            value = arguments[index + 1]
            if argument == "-object" and (
                value.startswith("rng-") or "id=host_rng0" in value.split(",")
            ):
                objects.append((index, value))
            if argument == "-device" and value.startswith("virtio-rng"):
                devices.append(value)
        if argument == "-netdev" or (argument == "-device" and index + 1 < len(arguments)
                                    and "vsock" in arguments[index + 1]):
            raise ValueError("network/vsock is forbidden")
    if len(objects) != 1 or objects[0][1] != "rng-random,id=host_rng0":
        raise ValueError("expected exactly one standard host_rng0 backend")
    if devices != ["virtio-rng-device,rng=host_rng0"]:
        raise ValueError("expected exactly one MMIO RNG device using host_rng0")
    index = objects[0][0]
    return arguments[:index] + [
        "-chardev", f"socket,id=pending_rng_gate,path={socket_path}",
        "-object", "rng-egd,id=host_rng0,chardev=pending_rng_gate",
    ] + arguments[index + 2:]


class EgdParser:
    """Only EGD blocking entropy requests (opcode 2, nonzero u8 length)."""

    def __init__(self):
        self.pending = bytearray()
        self.requests = 0
        self.requested_bytes = 0

    def feed(self, data):
        self.pending.extend(data)
        while self.pending:
            if self.pending[0] != 2:
                raise ValueError("unexpected EGD opcode")
            if len(self.pending) < 2:
                return
            length = self.pending[1]
            if length == 0:
                raise ValueError("zero-length EGD request")
            del self.pending[:2]
            self.requests += 1
            self.requested_bytes += length

    def finish(self):
        if self.pending:
            raise ValueError("truncated EGD request")


def validate_evidence(output, gate, make_returncode):
    lines = [line.strip() for line in output.splitlines()]
    if make_returncode != 0 or gate.get("qemu_returncode") != 0:
        raise ValueError("make/QEMU did not exit successfully")
    if gate.get("error") or gate.get("response_bytes") != 0:
        raise ValueError("EGD gate failed or sent data")
    if gate.get("requests", 0) <= 0 or gate.get("requested_bytes", 0) <= 0:
        raise ValueError("no EGD request observed")
    if lines.count(BLOCKED) != 1 or lines.count(CLOSED) != 1:
        raise ValueError("missing or duplicate guest markers")
    if lines.index(BLOCKED) >= lines.index(CLOSED):
        raise ValueError("guest markers reversed")
    if PASSED not in lines or "=== UNITTEST_STATUS: TESTS_FAILED ===" in lines:
        raise ValueError("guest suite did not pass")
    matches = [line for line in lines if re.fullmatch(
        rf"RESULT (?:[\w:]+::)?{TEST_NAME} \.\.\. ok", line
    )]
    if len(matches) != 1:
        raise ValueError("pending lifecycle test did not pass exactly once")


def validate_config(root):
    config = (root / ".config").read_text()
    values = dict(line.split("=", 1) for line in config.splitlines()
                  if "=" in line and not line.startswith("#"))
    for key in ("ARCH_AARCH64", "MACHINE_AARCH64_QEMU", "KFEAT_VIRTIO_BUS_MMIO",
                "KFEAT_DRIVER_VIRTIO_RNG"):
        if values.get(key) != "y":
            raise ValueError(f"required config: {key}=y")
    for key in ("KFEAT_ENTROPY_VIRTIO_RNG", "KFEAT_VIRTIO_BUS_PCI"):
        if values.get(key, "n") != "n":
            raise ValueError(f"must disable {key}")
    disk = root / "disk.img"
    if disk.is_symlink() or not disk.is_file() or disk.stat().st_nlink != 1:
        raise ValueError("disk.img must be a private regular file, not a symlink/hardlink")
    return config


def wrapper():
    real_qemu = os.environ["PENDING_REAL_QEMU"]
    arguments = sys.argv[1:]
    if "-kernel" not in arguments:
        os.execv(real_qemu, [real_qemu] + arguments)
    log_dir = Path(os.environ["PENDING_LOG_DIR"])
    with (log_dir / "invocation.json").open("x") as invocation:
        json.dump(arguments, invocation)
    parser = EgdParser()
    errors = []
    finished = threading.Event()
    with tempfile.TemporaryDirectory(prefix="rng-gate-") as temporary:
        socket_path = str(Path(temporary) / "egd.sock")
        rewritten = rewrite_argv(arguments, socket_path)
        (log_dir / "qemu-argv.json").write_text(json.dumps([real_qemu] + rewritten, indent=2))
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            server.bind(socket_path)
            server.listen(1)
            server.settimeout(0.1)

            def receive():
                try:
                    while not finished.is_set():
                        try:
                            connection, _ = server.accept()
                            break
                        except socket.timeout:
                            continue
                    else:
                        return
                    with connection:
                        connection.settimeout(0.1)
                        while True:
                            try:
                                data = connection.recv(4096)
                            except socket.timeout:
                                if finished.is_set():
                                    break
                                continue
                            if not data:
                                break
                            if not errors:
                                try:
                                    parser.feed(data)
                                except ValueError as error:
                                    errors.append(str(error))
                        if not errors:
                            parser.finish()
                except (OSError, ValueError) as error:
                    errors.append(str(error))

            receiver = threading.Thread(target=receive)
            receiver.start()
            try:
                with (log_dir / "guest.log").open("xb") as guest:
                    process = subprocess.Popen([real_qemu] + rewritten, stdout=subprocess.PIPE,
                                               stderr=subprocess.STDOUT)
                    try:
                        while chunk := process.stdout.read1(65536):
                            guest.write(chunk)
                            guest.flush()
                            sys.stdout.buffer.write(chunk)
                            sys.stdout.buffer.flush()
                        returncode = process.wait()
                    finally:
                        if process.poll() is None:
                            process.kill()
                        process.wait()
                        process.stdout.close()
            finally:
                finished.set()
                receiver.join()
    (log_dir / "gate.json").write_text(json.dumps({
        "requests": parser.requests, "requested_bytes": parser.requested_bytes,
        "response_bytes": 0, "error": errors, "qemu_returncode": returncode,
    }, indent=2))
    return returncode if not errors else 1


def timeout_seconds(value):
    seconds = float(value)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("timeout must be finite and positive")
    return seconds


def experiment_environment(environment, wrapper_dir, real_qemu, log_dir):
    environment = environment.copy()
    for key in ("XK_TEST_MODEL_MMIO_LIFECYCLE", "MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES"):
        environment.pop(key, None)
    environment.update({
        "XK_TEST_MODEL_MMIO_PENDING": "1", "XK_EXPECT_MODEL_MMIO_RNG": "1",
        "PENDING_REAL_QEMU": str(real_qemu), "PENDING_LOG_DIR": str(log_dir),
        "PATH": str(wrapper_dir) + os.pathsep + environment.get("PATH", ""),
    })
    return environment


def terminate_group(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def create_launcher(directory):
    wrapper_path = Path(directory) / "qemu-system-aarch64"
    wrapper_path.write_text(
        f"#!{sys.executable}\nimport runpy, sys\n"
        f"sys.exit(runpy.run_path({str(Path(__file__).resolve())!r})['wrapper']())\n"
    )
    wrapper_path.chmod(0o700)
    return wrapper_path


def main():
    argument_parser = argparse.ArgumentParser(description=__doc__)
    argument_parser.add_argument("--log-dir", type=Path, required=True)
    argument_parser.add_argument("--timeout-seconds", type=timeout_seconds, default=300)
    arguments = argument_parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    config = validate_config(root)
    resolved = shutil.which("qemu-system-aarch64")
    if resolved is None:
        raise ValueError("qemu-system-aarch64 is not on PATH")
    real_qemu = Path(resolved).resolve()
    log_dir = arguments.log_dir.resolve()
    log_dir.mkdir(parents=True, exist_ok=False)
    (log_dir / "config").write_text(config)
    metadata = {"root": str(root), "command": COMMAND, "real_qemu": str(real_qemu),
                "timeout_seconds": arguments.timeout_seconds, "started": time.time(),
                "success": False}
    (log_dir / "metadata.json").write_text(json.dumps(metadata, indent=2))
    try:
        with tempfile.TemporaryDirectory(prefix="rng-wrapper-") as temporary:
            create_launcher(temporary)
            environment = experiment_environment(os.environ, temporary, real_qemu, log_dir)
            with (log_dir / "make.log").open("xb") as log:
                process = subprocess.Popen(COMMAND, cwd=root, env=environment, stdout=log,
                                           stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    returncode = process.wait(timeout=arguments.timeout_seconds)
                finally:
                    terminate_group(process)
            gate = json.loads((log_dir / "gate.json").read_text())
            validate_evidence((log_dir / "guest.log").read_text(errors="replace"), gate, returncode)
            metadata["success"] = True
    except BaseException as error:
        metadata["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        metadata["finished"] = time.time()
        (log_dir / "metadata.json").write_text(json.dumps(metadata, indent=2))
    print(f"Pending RNG evidence accepted: {log_dir}")
    return 0


if __name__ == "__main__":
    try:
        if Path(sys.argv[0]).name == "qemu-system-aarch64":
            sys.exit(wrapper())
        sys.exit(main())
    except (OSError, ValueError, subprocess.TimeoutExpired) as failure:
        print(f"pending RNG harness: {failure}", file=sys.stderr)
        sys.exit(1)
