# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Host regressions for failed launch, timeout evidence and process cleanup."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

RUNNER = Path(__file__).with_name('qemu_s3.py')


class FailureEvidence(unittest.TestCase):
    def run_failure(self, program):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            tools = root / 'bin'
            tools.mkdir()
            make = tools / 'make'
            make.write_text('#!' + sys.executable + '\n' + program)
            make.chmod(0o755)
            output = root / 'evidence'
            output.mkdir()
            env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                       S3_OUTPUT_DIR=str(output), S3_TEST_UNSUPPORTED='1',
                       S3_TEST_ROUNDS='3', S3_TEST_TIMEOUT_SECONDS='1.5')
            result = subprocess.run([sys.executable, str(RUNNER)], env=env,
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            summary = json.loads((output / 'guest-e2e-result.json').read_text())
            self.assertFalse(summary['passed'])
            self.assertTrue(summary['error'])
            summary['serial_tail'] = (output / 'guest-e2e-serial.log').read_text()[-1000:]
            self.assertTrue((output / 'guest-e2e-serial.log').exists())
            self.assertEqual(json.loads((output / 'guest-e2e-clock-checks.json').read_text()), [])
            self.assertTrue((output / 'guest-e2e-qmp.json').exists())
            self.assertFalse((output / 'qmp.sock').exists())
            pid_file = output / 'child.pid'
            if pid_file.exists():
                with self.assertRaises(ProcessLookupError):
                    os.kill(int(pid_file.read_text()), 0)
            return summary

    def test_launch_exit_still_writes_failed_verdict(self):
        summary = self.run_failure('raise SystemExit(17)\n')
        self.assertEqual(summary['make_exit_code'], 17)
        self.assertIn('QMP socket', summary['error'])

    def test_hang_is_terminated_and_retains_failed_verdict(self):
        summary = self.run_failure('''import json, os, socket
from pathlib import Path
out = Path(os.environ['S3_OUTPUT_DIR'])
(out / 'child.pid').write_text(str(os.getpid()))
server = socket.socket(socket.AF_UNIX)
server.bind(str(out / 'qmp.sock'))
server.listen(1)
conn, _ = server.accept()
conn.sendall(b'{"QMP": {}}\\n')
while True:
    data = conn.recv(4096)
    if not data:
        break
    conn.sendall(b'{"return": {}}\\n')
''')
        self.assertIn('bounded test window', summary['error'], summary['serial_tail'])
        self.assertLess(summary['make_exit_code'], 0)

    def test_shell_announces_evidence_before_setup_failure(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            scripts = root / 'scripts' / 'pm'
            scripts.mkdir(parents=True)
            shutil.copy(RUNNER.with_name('run_qemu_s3.sh'), scripts)
            shutil.copy(RUNNER.with_name('qemu_s3_defconfig'), scripts)
            tools = root / 'bin'
            tools.mkdir()
            make = tools / 'make'
            make.write_text('#!/bin/sh\nexit 17\n')
            make.chmod(0o755)
            output = root / 'evidence'
            env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                       S3_OUTPUT_DIR=str(output))
            result = subprocess.run(['bash', str(scripts / 'run_qemu_s3.sh')],
                                    env=env, capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 17)
            self.assertEqual(result.stdout.count('Evidence: ' + str(output)), 2)


if __name__ == '__main__':
    unittest.main()
