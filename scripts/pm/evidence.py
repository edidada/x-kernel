# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Strict protocol checks shared by real runs and adversarial host fixtures."""
import re

STAGES = (
    'guest suspend: tasks and normal queue frozen',
    'guest suspend: device callbacks completed',
    'guest suspend: {remote} remote CPUs acknowledged',
    'guest suspend: entering platform S3',
    'guest resume: platform returned, sleep accounting Ok(',
    'guest resume: CPUs, devices, queues and tasks restored',
)


def protocol_checks(serial, events, clocks, *, arch, cpus, rounds, refusal):
    """Require exact cycles, paired hardware events and the production chain.

    A refusal can pass its own negative scenario but never a resume scenario.
    Clock comparisons are computed by the live runner against host time.
    """
    expected = list(range(1, rounds + 1))
    requests = [int(x) for x in re.findall(r'^E2E_CYCLE (\d+) REQUEST$', serial, re.M)]
    marker = 'REFUSAL' if refusal else 'CYCLE'
    passed = [int(x) for x in re.findall(r'^E2E_' + marker + r' (\d+) PASS(?: .*|)$', serial, re.M)]
    transitions = [e.get('event') for e in events if e.get('event') in ('SUSPEND', 'WAKEUP')]
    reset = any(e.get('event') == 'RESET'
                or (e.get('event') == 'SHUTDOWN'
                    and e.get('data', {}).get('reason') == 'guest-reset') for e in events)
    qmp_errors = [e['error'] for e in events if 'error' in e]
    stages_ok = not any(s.split('{')[0] in serial for s in STAGES) if refusal else True
    if not refusal:
        # Each chain must lie inside its own request/return boundary. Global
        # stage counts alone could accept duplicated or reordered cycles.
        chunks = re.split(r'^E2E_CYCLE \d+ REQUEST$', serial, flags=re.M)[1:]
        stages_ok = len(chunks) == rounds
        for chunk in chunks:
            returned = re.search(r'^E2E_CLOCKS ', chunk, re.M)
            if returned is None:
                stages_ok = False
                continue
            prefix = chunk[:returned.start()]
            cursor = 0
            for stage in STAGES:
                stage = stage.format(remote=cpus - 1)
                at = prefix.find(stage, cursor)
                if at < 0 or prefix.count(stage) != 1:
                    stages_ok = False
                    break
                cursor = at + len(stage)
    clock_ok = (len(clocks) == rounds
                and [c['cycle'] for c in clocks] == expected
                and all(c['passed'] for c in clocks))
    final = 'E2E_REFUSAL_PASS' if refusal else 'E2E_SUSPEND_PASS'
    checks = {
        'architecture_matches': re.findall(r'^E2E_ARCH (\S+)$', serial, re.M) == [arch],
        'exact_requests': requests == expected,
        'exact_passed_cycles': passed == expected,
        'guest_completed': re.findall(r'^' + final + r'$', serial, re.M) == [final],
        'no_guest_failure': 'E2E_FAIL:' not in serial and 'E2E_REBOOT_ERROR' not in serial,
        'no_kernel_failure': re.search(r'panicked at|kernel panic|Backtrace:', serial, re.I) is None,
        'no_qmp_error': not qmp_errors,
        'no_guest_reset': not reset,
        'hardware_events_match': transitions == ([] if refusal else ['SUSPEND', 'WAKEUP'] * rounds),
        'production_stages_match': stages_ok,
        'clock_compensation_matches_host': refusal or clock_ok,
    }
    return checks
