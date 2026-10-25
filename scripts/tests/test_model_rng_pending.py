"""Host tests: python3 -m unittest discover -s scripts/tests -p 'test_model_rng_pending.py'."""

import argparse
import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch


SPEC = importlib.util.spec_from_file_location(
    "pending_rng", Path(__file__).with_name("run_model_rng_pending.py")
)
harness = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(harness)


class ArgumentTests(unittest.TestCase):
    def setUp(self):
        self.arguments = [
            "-kernel", "kernel.elf", "-object", "rng-random,id=host_rng0",
            "-device", "virtio-rng-device,rng=host_rng0", "-global",
            "virtio-mmio.force-legacy=false",
        ]

    def test_replaces_backend_without_adding_device(self):
        rewritten = harness.rewrite_argv(self.arguments, "/tmp/gate.sock")
        self.assertEqual(rewritten, [
            "-kernel", "kernel.elf", "-chardev",
            "socket,id=pending_rng_gate,path=/tmp/gate.sock", "-object",
            "rng-egd,id=host_rng0,chardev=pending_rng_gate", "-device",
            "virtio-rng-device,rng=host_rng0", "-global",
            "virtio-mmio.force-legacy=false",
        ])
        self.assertEqual(self.arguments[3], "rng-random,id=host_rng0")

    def test_missing_duplicate_or_wrong_backend_and_device_rejected(self):
        cases = [
            self.arguments[:2] + self.arguments[4:],
            self.arguments[:4],
            self.arguments + self.arguments[2:4],
            self.arguments + self.arguments[4:6],
            self.arguments + ["-object", "rng-random,id=another"],
            [value.replace("virtio-rng-device", "virtio-rng-pci") for value in self.arguments],
            [value.replace("rng-random", "rng-egd") for value in self.arguments],
            self.arguments + ["-device"],
            self.arguments + ["-netdev", "user,id=net0"],
            self.arguments + ["-device", "vhost-vsock-device"],
            self.arguments + ["-kernel", "other.elf"],
        ]
        for arguments in cases:
            with self.subTest(arguments=arguments), self.assertRaises(ValueError):
                harness.rewrite_argv(arguments, "/tmp/gate.sock")

    def test_help_probe_execs_real_binary_unchanged(self):
        with patch.object(harness.sys, "argv", ["qemu-system-aarch64", "-device", "help"]), \
                patch.dict(harness.os.environ, {"PENDING_REAL_QEMU": "/real/qemu"}), \
                patch.object(harness.os, "execv", side_effect=SystemExit) as execute:
            with self.assertRaises(SystemExit):
                harness.wrapper()
            execute.assert_called_once_with("/real/qemu", ["/real/qemu", "-device", "help"])


class ParserTests(unittest.TestCase):
    def test_fragmented_and_batched_requests(self):
        parser = harness.EgdParser()
        for data in (b"\x02", b"\x08\x02\xff\x02", b"\x01"):
            parser.feed(data)
        parser.finish()
        self.assertEqual((parser.requests, parser.requested_bytes), (3, 264))

    def test_bad_opcode_zero_length_and_truncation(self):
        for data in (b"\x01\x08", b"\x02\x00", b"\x03", b"\x02\x01\x99"):
            with self.subTest(data=data), self.assertRaises(ValueError):
                harness.EgdParser().feed(data)
        parser = harness.EgdParser()
        parser.feed(b"\x02")
        with self.assertRaises(ValueError):
            parser.finish()


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.output = "\n".join([
            harness.BLOCKED, harness.CLOSED,
            f"      RESULT kdriver::platform_model::pending_tests::{harness.TEST_NAME} ... ok\r",
            harness.PASSED + "\r",
        ])
        self.gate = {"requests": 1, "requested_bytes": 64, "response_bytes": 0,
                     "error": [], "qemu_returncode": 0}

    def test_accepts_counts_without_cross_fd_timestamp_assumptions(self):
        harness.validate_evidence(self.output, self.gate, 0)

    def test_previous_lifecycle_log_is_not_pending_test_evidence(self):
        previous_result = (
            "      RESULT kdriver::platform_model::lifecycle_tests::"
            "real_model_mmio_rng_unbind_rebind_closes_retained_publication ... ok\r\n"
        )
        output = "\n".join([harness.BLOCKED, harness.CLOSED, previous_result, harness.PASSED])
        with self.assertRaises(ValueError):
            harness.validate_evidence(output, self.gate, 0)

    def test_rejects_missing_duplicate_reversed_or_failed_guest_evidence(self):
        cases = [
            self.output.replace(harness.BLOCKED, ""),
            self.output + "\n" + harness.CLOSED,
            self.output.replace(harness.BLOCKED, "temporary").replace(
                harness.CLOSED, harness.BLOCKED).replace("temporary", harness.CLOSED),
            self.output.replace(" ... ok", " ... ignored"),
            self.output.replace(" ... ok", " ... FAILED"),
            self.output.replace(harness.PASSED, ""),
            self.output + "\n=== UNITTEST_STATUS: TESTS_FAILED ===",
            self.output.replace(harness.BLOCKED, "prefix " + harness.BLOCKED),
        ]
        for output in cases:
            with self.subTest(output=output), self.assertRaises(ValueError):
                harness.validate_evidence(output, self.gate, 0)

    def test_rejects_no_request_response_or_process_failure(self):
        for key, value in (("requests", 0), ("requested_bytes", 0), ("response_bytes", 1),
                           ("error", ["bad opcode"]), ("qemu_returncode", 1)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                harness.validate_evidence(self.output, dict(self.gate, **{key: value}), 0)
        with self.assertRaises(ValueError):
            harness.validate_evidence(self.output, self.gate, 2)


class SetupTests(unittest.TestCase):
    def test_existing_log_directory_is_refused_before_launch(self):
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(harness.sys, "argv", ["runner", "--log-dir", temporary]), \
                patch.object(harness, "validate_config", return_value="config"), \
                patch.object(harness.shutil, "which", return_value="/real/qemu"), \
                patch.object(harness.subprocess, "Popen") as launch:
            with self.assertRaises(FileExistsError):
                harness.main()
            launch.assert_not_called()

    def test_generated_launcher_passthrough_and_kernel_socket(self):
        for malformed in (False, True):
            with self.subTest(malformed=malformed), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                logs = root / "logs"
                logs.mkdir()
                real = root / "stub-qemu"
                real.write_text(
                    f"#!{sys.executable}\n"
                    "import socket, sys\n"
                    "if '-kernel' not in sys.argv:\n"
                    "    print('stub version')\n"
                    "    sys.exit(0)\n"
                    "spec = sys.argv[sys.argv.index('-chardev') + 1]\n"
                    "path = dict(part.split('=', 1) for part in spec.split(',')[1:])['path']\n"
                    "with socket.socket(socket.AF_UNIX) as peer:\n"
                    "    peer.connect(path)\n"
                    f"    peer.sendall({b'\x03\x08' if malformed else b'\x02\x08'!r})\n"
                    "    peer.settimeout(0.15)\n"
                    "    try:\n"
                    "        peer.recv(1)\n"
                    "        sys.exit('unexpected response or EOF')\n"
                    "    except socket.timeout:\n"
                    "        print('stub socket stayed open without response')\n"
                )
                real.chmod(0o700)
                launcher = harness.create_launcher(root)
                environment = harness.experiment_environment(os.environ, root, real, logs)
                version = subprocess.run([str(launcher), "--version"], env=environment,
                                         capture_output=True, text=True, timeout=5)
                self.assertEqual(version.returncode, 0, version.stderr)
                self.assertEqual(version.stdout, "stub version\n")
                self.assertFalse((logs / "gate.json").exists())
                result = subprocess.run([
                    str(launcher), "-kernel", "fake.elf", "-object", "rng-random,id=host_rng0",
                    "-device", "virtio-rng-device,rng=host_rng0",
                ], env=environment, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, int(malformed), result.stderr)
                self.assertEqual(result.stdout, "stub socket stayed open without response\n")
                self.assertEqual((logs / "guest.log").read_text(), result.stdout)
                gate = json.loads((logs / "gate.json").read_text())
                self.assertEqual(gate["response_bytes"], 0)
                self.assertEqual(bool(gate["error"]), malformed)
                if not malformed:
                    self.assertEqual((gate["requests"], gate["requested_bytes"]), (1, 8))

    def test_flags_are_scoped_and_lifecycle_is_removed(self):
        original = {"PATH": "/usr/bin", "XK_TEST_MODEL_MMIO_LIFECYCLE": "1",
                    "XK_TEST_MODEL_MMIO_ISOLATION": "1",
                    "XK_TEST_MODEL_MMIO_UNREGISTER": "1",
                    "MAKEFLAGS": "inherited"}
        environment = harness.experiment_environment(original, "/wrapper", "/real/qemu", "/logs")
        self.assertNotIn("XK_TEST_MODEL_MMIO_LIFECYCLE", environment)
        self.assertNotIn("XK_TEST_MODEL_MMIO_ISOLATION", environment)
        self.assertNotIn("XK_TEST_MODEL_MMIO_UNREGISTER", environment)
        self.assertNotIn("MAKEFLAGS", environment)
        self.assertEqual(environment["XK_TEST_MODEL_MMIO_PENDING"], "1")
        self.assertEqual(environment["XK_EXPECT_MODEL_MMIO_RNG"], "1")
        self.assertEqual(environment["PATH"], "/wrapper:/usr/bin")
        self.assertIn("XK_TEST_MODEL_MMIO_LIFECYCLE", original)

    def test_timeout_validation(self):
        self.assertEqual(harness.timeout_seconds("300"), 300)
        for value in ("0", "-1", "nan", "inf"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                harness.timeout_seconds(value)

    def test_timeout_targets_whole_owned_group_even_if_leader_exits(self):
        process = Mock(pid=12345)
        process.wait.side_effect = [subprocess.TimeoutExpired("make", 2), 0]
        with patch.object(harness.os, "killpg") as kill:
            harness.terminate_group(process)
        self.assertEqual(kill.call_args_list[0].args, (12345, signal.SIGTERM))
        self.assertEqual(kill.call_args_list[1].args, (12345, signal.SIGKILL))
        self.assertEqual(process.wait.call_count, 2)

    def test_config_and_private_disk_requirements(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = "\n".join(f"{key}=y" for key in (
                "ARCH_AARCH64", "MACHINE_AARCH64_QEMU", "KFEAT_VIRTIO_BUS_MMIO",
                "KFEAT_DRIVER_VIRTIO_RNG"))
            (root / ".config").write_text(config)
            (root / "disk.img").write_bytes(b"test fixture")
            self.assertEqual(harness.validate_config(root), config)
            (root / ".config").write_text(config + "\nKFEAT_ENTROPY_VIRTIO_RNG=y")
            with self.assertRaises(ValueError):
                harness.validate_config(root)
            (root / ".config").write_text(config)
            (root / "disk.img").unlink()
            (root / "target").write_bytes(b"fixture")
            (root / "disk.img").symlink_to(root / "target")
            with self.assertRaises(ValueError):
                harness.validate_config(root)


if __name__ == "__main__":
    unittest.main()
