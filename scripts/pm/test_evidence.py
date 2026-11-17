# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Adversarial verdict tests: a partial chain must never look like real sleep."""
import unittest

from evidence import STAGES, protocol_checks


class EvidenceProtocol(unittest.TestCase):
    def setUp(self):
        self.serial = 'E2E_ARCH x86_64\n'
        self.events = []
        self.clocks = []
        for cycle in (1, 2, 3):
            self.serial += f'E2E_CYCLE {cycle} REQUEST\n'
            self.serial += '\n'.join(s.format(remote=3) for s in STAGES) + '\n'
            self.serial += f'E2E_CLOCKS cycle={cycle} mono_ns=1 boot_ns=3000000000 real_ns=3000000000\n'
            self.serial += f'E2E_CYCLE {cycle} PASS mono_ns=1 sleep_ns=3000000000\n'
            self.events += [{'event': 'SUSPEND'}, {'event': 'WAKEUP'}]
            self.clocks.append({'cycle': cycle, 'passed': True})
        self.serial += 'E2E_SUSPEND_PASS\n'

    def checks(self, **changes):
        args = dict(serial=self.serial, events=self.events, clocks=self.clocks,
                    arch='x86_64', cpus=4, rounds=3, refusal=False)
        args.update(changes)
        return protocol_checks(**args)

    def test_complete_production_chain_is_accepted(self):
        self.assertTrue(all(self.checks().values()))

    def test_missing_or_duplicate_cycle_cannot_pass(self):
        for serial in (self.serial.replace('E2E_CYCLE 2 PASS', 'E2E_CYCLE 20 PASS'),
                       self.serial + 'E2E_CYCLE 3 PASS\n',
                       self.serial.replace('E2E_CYCLE 2 REQUEST\n', '')):
            with self.subTest(serial=serial):
                self.assertFalse(all(self.checks(serial=serial).values()))

    def test_missing_stage_or_wrong_remote_count_cannot_pass(self):
        for stage in STAGES:
            self.assertFalse(all(self.checks(serial=self.serial.replace(
                stage.format(remote=3), 'omitted', 1)).values()))
        self.assertFalse(all(self.checks(cpus=1).values()))

    def test_reordered_resume_stages_cannot_pass(self):
        serial = self.serial.replace(STAGES[-1], 'temporary').replace(
            STAGES[-2], STAGES[-1]).replace('temporary', STAGES[-2])
        self.assertFalse(all(self.checks(serial=serial).values()))

    def test_stop_cont_does_not_count_as_platform_suspend(self):
        events = [{'event': x} for x in ('STOP', 'RESUME') * 3]
        self.assertFalse(all(self.checks(events=events).values()))

    def test_counts_with_unpaired_events_cannot_pass(self):
        events = [{'event': x} for x in ('SUSPEND',) * 3 + ('WAKEUP',) * 3]
        self.assertFalse(all(self.checks(events=events).values()))

    def test_reset_or_qmp_error_cannot_pass(self):
        for bad in ({'event': 'RESET'}, {'error': {'class': 'GenericError'}},
                    {'event': 'SHUTDOWN', 'data': {'reason': 'guest-reset'}}):
            self.assertFalse(all(self.checks(events=self.events + [bad]).values()))

    def test_wrong_arch_or_guest_failure_cannot_pass(self):
        self.assertFalse(all(self.checks(arch='aarch64').values()))
        self.assertFalse(all(self.checks(serial=self.serial + 'E2E_FAIL: timer\n').values()))
        self.assertFalse(all(self.checks(serial=self.serial + 'panicked at recovery.rs:1\n').values()))

    def test_missing_duplicate_or_failed_host_clock_check_cannot_pass(self):
        for clocks in (self.clocks[:-1], self.clocks + self.clocks[:1],
                       [{'cycle': x, 'passed': x != 2} for x in (1, 2, 3)]):
            self.assertFalse(all(self.checks(clocks=clocks).values()))

    def test_refusal_cannot_be_accepted_as_real_resume(self):
        serial = 'E2E_ARCH aarch64\n'
        for cycle in (1, 2, 3):
            serial += f'E2E_CYCLE {cycle} REQUEST\nE2E_REFUSAL {cycle} PASS\n'
        serial += 'E2E_REFUSAL_PASS\n'
        args = dict(serial=serial, events=[], clocks=[], arch='aarch64')
        self.assertTrue(all(self.checks(**args, refusal=True).values()))
        self.assertFalse(all(self.checks(**args).values()))
        self.assertFalse(all(self.checks(**args, refusal=True, rounds=4).values()))
        self.assertFalse(all(self.checks(**dict(args, serial=serial + STAGES[0]),
                                        refusal=True).values()))


if __name__ == '__main__':
    unittest.main()
