#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Execute separate real-resume and unsupported-refusal QEMU scenarios.

All requested scenarios are required. Missing backends/emulators fail the
matrix, rather than silently skipping or converting a resume into a refusal.
Run sequentially: the production build consumes the worktree's .config.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import xml.etree.ElementTree as ET

REPO = Path(__file__).resolve().parents[2]
ARCHES = ('x86_64', 'aarch64', 'riscv64')


def metadata(arch):
    """Keep source/build identity with results, including dirty probe inputs."""
    def git(*args):
        return subprocess.check_output(['git', *args], cwd=REPO, text=True).strip()
    qemu = 'qemu-system-' + arch
    binary = shutil.which(qemu)
    version = None
    qemu_error = None
    if binary is not None:
        try:
            version = subprocess.check_output([binary, '--version'], text=True,
                                              timeout=10, stderr=subprocess.STDOUT).splitlines()[0]
        except (OSError, subprocess.SubprocessError, IndexError) as error:
            qemu_error = str(error)
    return {
        'git_commit': git('rev-parse', 'HEAD'),
        'git_status': git('status', '--short'),
        'qemu': binary,
        'qemu_version': version,
        'qemu_error': qemu_error,
        'test_sources_sha256': {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in Path(__file__).parent.iterdir()
            if path.suffix in ('.rs', '.py', '.sh') or path.name.endswith('defconfig')
        },
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--arches', nargs='+', choices=ARCHES, default=list(ARCHES))
    parser.add_argument('--cpus', nargs='+', type=int, choices=(1, 4), default=[1, 4])
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--scenario', choices=('all', 'resume', 'refusal'), default='all')
    parser.add_argument('--output', type=Path)
    parser.add_argument('--case-timeout', type=int, default=1800,
                        help='seconds including kernel build; timeout is always failure')
    args = parser.parse_args()
    if not 1 <= args.rounds <= 100 or not 1 <= args.case_timeout <= 7200:
        parser.error('rounds must be 1..100 and case-timeout 1..7200')
    output = args.output or Path(tempfile.mkdtemp(prefix='xkernel-pm-matrix-'))
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        parser.error('output must be empty; existing evidence is never overwritten')
    print('Evidence:', output, flush=True)
    scenarios = ['resume', 'refusal', 'vga-refusal'] if args.scenario == 'all' else [args.scenario]
    required_cases = sum(len(set(args.cpus)) for arch in set(args.arches)
                         for scenario in scenarios
                         if scenario != 'vga-refusal' or arch == 'x86_64')
    results = []
    junit = ET.Element('testsuite', name='guest-system-suspend-resume')
    (output / 'matrix-result.json').write_text(json.dumps(dict(
        schema_version=1, required_cases=required_cases, complete=False,
        passed=False, all_architectures_resumed=False, results=[]), indent=2) + '\n')
    for arch in dict.fromkeys(args.arches):
        identity = metadata(arch)
        for cpus in dict.fromkeys(args.cpus):
            for scenario in scenarios:
                if scenario == 'vga-refusal' and arch != 'x86_64':
                    continue  # This is an explicitly x86-only hardware profile.
                name = f'{arch}-{cpus}cpu-{scenario}'
                case = output / name
                case.mkdir()
                (case / 'provenance.json').write_text(json.dumps(identity, indent=2) + '\n')
                env = dict(os.environ, S3_TEST_ARCH=arch, S3_TEST_CPUS=str(cpus),
                           S3_TEST_ROUNDS=str(args.rounds), S3_OUTPUT_DIR=str(case),
                           S3_TEST_UNSUPPORTED='0' if scenario == 'resume' else '1',
                           S3_TEST_VGA='1' if scenario == 'vga-refusal' else '0')
                # Per-architecture firmware overrides must not leak into another
                # architecture's command. Values remain recorded in launch_command.
                env['S3_TEST_QEMU_ARGS'] = os.environ.get('PM_QEMU_ARGS_' + arch.upper(), '')
                started = time.monotonic()
                error = None
                code = None
                if identity['qemu'] is None or identity.get('qemu_error'):
                    error = identity.get('qemu_error') or 'required QEMU emulator not found'
                else:
                    with (case / 'runner.log').open('w') as log:
                        # The inner QMP runner owns its own process-group cleanup.
                        # A build timeout must also terminate shell/compiler children.
                        proc = subprocess.Popen(['bash', str(Path(__file__).with_name('run_qemu_s3.sh'))],
                                                cwd=REPO, env=env, stdout=log,
                                                stderr=subprocess.STDOUT, start_new_session=True)
                        try:
                            code = proc.wait(timeout=args.case_timeout)
                        except subprocess.TimeoutExpired:
                            try:
                                os.killpg(proc.pid, signal.SIGTERM)
                            except ProcessLookupError:
                                pass
                            try:
                                proc.wait(timeout=10)
                            except subprocess.TimeoutExpired:
                                try:
                                    os.killpg(proc.pid, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass
                                proc.wait()
                            error = 'case timed out (including build)'
                result_file = case / 'guest-e2e-result.json'
                try:
                    result = json.loads(result_file.read_text()) if result_file.exists() else {}
                except (OSError, ValueError) as exc:
                    result = {}
                    error = str(exc)
                identity_matches = (result.get('architecture') == arch
                                    and result.get('cpus') == cpus
                                    and result.get('rounds') == args.rounds)
                actual_resume = result.get('actual_suspend_resume') is True
                passed = (code == 0 and error is None and result.get('passed') is True
                          and identity_matches and actual_resume == (scenario == 'resume'))
                if not passed:
                    error = error or result.get('error') or 'guest assertions or build failed; inspect runner.log'
                record = dict(name=name, architecture=arch, cpus=cpus, scenario=scenario,
                              passed=passed, actual_suspend_resume=passed and scenario == 'resume',
                              runner_exit_code=code, error=error, evidence=str(case),
                              seconds=time.monotonic() - started, result=result)
                results.append(record)
                node = ET.SubElement(junit, 'testcase', name=name,
                                     classname=arch, time=str(record['seconds']))
                if not passed:
                    ET.SubElement(node, 'failure', message=error).text = str(case)
                # Publish after every scenario, even if the host is interrupted.
                complete = len(results) == required_cases
                summary = dict(schema_version=1, results=results,
                               required_cases=required_cases, complete=complete,
                               passed=complete and all(r['passed'] for r in results),
                               all_architectures_resumed=(complete and set(args.arches) == set(ARCHES)
                                   and args.scenario != 'refusal'
                                   and all(any(r['architecture'] == a and r['cpus'] == c
                                               and r['actual_suspend_resume'] for r in results)
                                           for a in ARCHES for c in args.cpus)))
                (output / 'matrix-result.json').write_text(json.dumps(summary, indent=2) + '\n')
                junit.set('tests', str(len(results)))
                junit.set('failures', str(sum(not r['passed'] for r in results)))
                ET.ElementTree(junit).write(output / 'matrix-junit.xml', encoding='utf-8', xml_declaration=True)
                print(name, 'PASS' if passed else 'FAIL', error or '', flush=True)
    return 0 if summary['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
