# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Matrix contract tests; controlled launchers never emulate real platform sleep."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import run_qemu_suspend_matrix as matrix


class MatrixVerdict(unittest.TestCase):
    def run_matrix(self, *, missing=False, refusal_only=False, all_resume=False):
        snapshots = []
        class Launcher:
            def __init__(self, _args, *, env, **_kwargs):
                output = Path(env['S3_OUTPUT_DIR'])
                summary = output.parent / 'matrix-result.json'
                if summary.exists():
                    snapshots.append(json.loads(summary.read_text()))
                refusal = env['S3_TEST_UNSUPPORTED'] == '1'
                passed = refusal or all_resume
                result = dict(architecture=env['S3_TEST_ARCH'], cpus=int(env['S3_TEST_CPUS']),
                              rounds=int(env['S3_TEST_ROUNDS']), passed=passed,
                              actual_suspend_resume=passed and not refusal)
                (output / 'guest-e2e-result.json').write_text(json.dumps(result))
            def wait(self, **_kwargs):
                return 0
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'results'
            argv = ['matrix', '--output', str(output), '--rounds', '1']
            if refusal_only:
                argv += ['--scenario', 'refusal']
            with patch('sys.argv', argv), patch.object(matrix, 'metadata', return_value={
                    'qemu': None if missing else 'controlled-host-fixture'}), \
                    patch.object(matrix.subprocess, 'Popen', Launcher), \
                    contextlib.redirect_stdout(io.StringIO()):
                code = matrix.main()
            result = json.loads((output / 'matrix-result.json').read_text())
            self.assertTrue((output / 'matrix-junit.xml').exists())
            return code, result, snapshots

    def test_missing_emulators_fail_every_required_case(self):
        code, result, _ = self.run_matrix(missing=True)
        self.assertEqual(code, 1)
        self.assertTrue(result['complete'])
        self.assertEqual(result['required_cases'], 14)
        self.assertFalse(result['passed'])
        self.assertFalse(result['all_architectures_resumed'])

    def test_refusal_success_cannot_cover_failed_resume_scenarios(self):
        code, result, snapshots = self.run_matrix()
        self.assertEqual(code, 1)
        self.assertFalse(result['passed'])
        self.assertFalse(result['all_architectures_resumed'])
        for prior in snapshots:
            self.assertFalse(prior['complete'])
            self.assertFalse(prior['passed'])

    def test_refusal_only_matrix_does_not_claim_any_arch_resumed(self):
        code, result, _ = self.run_matrix(refusal_only=True)
        self.assertEqual(code, 0)
        self.assertTrue(result['passed'])
        self.assertFalse(result['all_architectures_resumed'])
        self.assertEqual(len(result['results']), 6)

    def test_all_required_real_scenarios_are_needed_for_green_matrix(self):
        code, result, snapshots = self.run_matrix(all_resume=True)
        self.assertEqual(code, 0)
        self.assertTrue(result['all_architectures_resumed'])
        self.assertEqual(len(result['results']), 14)
        self.assertTrue(all(not s['passed'] for s in snapshots))


if __name__ == '__main__':
    unittest.main()
