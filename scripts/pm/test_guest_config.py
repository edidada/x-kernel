# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Compile and run the guest's actual parser as a native host regression."""
from pathlib import Path
import subprocess
import tempfile
import unittest


class GuestConfiguration(unittest.TestCase):
    def test_malformed_overflowing_and_out_of_range_counts_do_not_panic(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'config-tests'
            subprocess.run(['rustc', '--edition=2024', '--test',
                            str(Path(__file__).with_name('ut-tests') / 'config.rs'),
                            '-o', str(output)], check=True, capture_output=True, timeout=30)
            subprocess.run([str(output)], check=True, capture_output=True, timeout=10)


if __name__ == '__main__':
    unittest.main()
