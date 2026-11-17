# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Boot a userspace suspend probe and capture real QMP transitions."""
import json
import hashlib
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time

from evidence import protocol_checks
from run_qemu_suspend_matrix import metadata

OUT = Path(os.environ["S3_OUTPUT_DIR"]).resolve()
REPO = Path(__file__).resolve().parents[2]
QMP_DIR = tempfile.TemporaryDirectory(prefix='xkernel-pm-qmp-')
QMP = Path(QMP_DIR.name) / 'qmp.sock'
QMP.unlink(missing_ok=True)
ARCH = os.environ.get('S3_TEST_ARCH', 'x86_64')
CPUS = int(os.environ.get('S3_TEST_CPUS', '4'))
PREFIX = 'guest-e2e'
for suffix in ('result.json', 'qmp-live.jsonl'):
    (OUT / f'{PREFIX}-{suffix}').unlink(missing_ok=True)
commands = ['make', 'justrun', 'ACCEL=n', 'SMP=' + os.environ.get('S3_TEST_CPUS', '4'),
            'DISK_IMG=' + str(OUT / 'root.img'),
            'XKMAKE_ARGS=--no-net --no-vsock',
            f'QEMU_ARGS=-no-reboot -qmp unix:{QMP},server=on,wait=off']
commands[-1] += ' -no-shutdown -nic none'
if ARCH == 'x86_64' and os.environ.get('S3_TEST_VGA') != '1':
    commands[-1] += ' -vga none'
if (ARCH == 'x86_64' and os.environ.get('S3_TEST_UNSUPPORTED') == '1'
        and os.environ.get('S3_TEST_VGA') != '1'):
    commands[-1] += ' -global ICH9-LPC.disable_s3=1'
commands[-1] += ' ' + os.environ.get('S3_TEST_QEMU_ARGS', '')
records = []
cycle_starts = {}
clock_checks = []
serial_lines_seen = 0
# One-second CMOS quantization plus at most two 300 ms serial polling edges.
# The remainder covers bounded marker transmission and clock sampling skew.
clock_tolerance_ns = 2_000_000_000

try:
    provenance = metadata(ARCH)
    provenance['inputs_sha256'] = {
        str(path.relative_to(OUT)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (OUT / 'kernel.config', OUT / 'root/bin/sh') if path.exists()
    }
    target = Path(os.environ.get('TARGET_DIR', str(REPO / 'target')))
    if not target.is_absolute():
        target = REPO / target
    kernel = target / 'xkmake' / ('kplat-' + ARCH) / 'release/kernel.elf'
    provenance['kernel_elf_sha256'] = (hashlib.sha256(kernel.read_bytes()).hexdigest()
                                       if kernel.exists() else None)
    (OUT / f'{PREFIX}-provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
except (OSError, subprocess.SubprocessError) as error:
    # Retain the provenance failure; the launch's ordinary evidence path below
    # still runs and writes its verdict instead of losing all diagnostics.
    provenance = {'error': str(error)}
    (OUT / f'{PREFIX}-provenance.json').write_text(json.dumps(provenance) + '\n')

def observe_serial():
    global serial_lines_seen
    lines = (OUT / f'{PREFIX}-serial.log').read_text(errors='replace').splitlines(keepends=True)
    complete = [line for line in lines if line.endswith('\n')]
    for line in complete[serial_lines_seen:]:
        now = time.monotonic_ns()
        request = re.search(r'E2E_CYCLE (\d+) REQUEST', line)
        if request:
            cycle_starts[int(request[1])] = now
        clocks = re.search(r'E2E_CLOCKS cycle=(\d+) mono_ns=(\d+) boot_ns=(\d+) real_ns=(\d+)', line)
        if clocks:
            cycle, mono, boot, real = map(int, clocks.groups())
            started = cycle_starts.get(cycle)
            elapsed = None if started is None else now - started
            clock_checks.append({
                'cycle': cycle, 'host_elapsed_ns': elapsed, 'monotonic_ns': mono,
                'boottime_ns': boot, 'realtime_ns': real,
                'passed': (elapsed is not None
                           and abs(boot - elapsed) <= clock_tolerance_ns
                           and abs(real - elapsed) <= clock_tolerance_ns),
            })
    serial_lines_seen = len(complete)

failure = provenance.get('error') or provenance.get('qemu_error')
proc = None

def interrupted(_signal, _frame):
    # An outer matrix timeout must run the inner launcher's finally cleanup;
    # the QEMU/make child has its own process group.
    raise RuntimeError('runner interrupted by host')

signal.signal(signal.SIGTERM, interrupted)
with (OUT / f'{PREFIX}-serial.log').open('w') as serial:
    sock = None
    try:
        proc = subprocess.Popen(commands, cwd=REPO, stdout=serial,
                                env=dict(os.environ, S3_QMP_SOCKET=str(QMP)),
                                stderr=subprocess.STDOUT, start_new_session=True)
    except OSError as error:
        failure = str(error)
    suspended_at = None
    woke = False
    deadline = time.monotonic() + 600
    try:
        timeout = float(os.environ.get("S3_TEST_TIMEOUT_SECONDS", "600"))
        if not 0 < timeout <= 600:
            raise ValueError("S3_TEST_TIMEOUT_SECONDS must be positive and at most 600")
        deadline = time.monotonic() + timeout
        if proc is None:
            raise RuntimeError(failure)
        while not QMP.exists() and proc.poll() is None and time.monotonic() < deadline:
            time.sleep(0.1)
        if not QMP.exists():
            raise RuntimeError('QMP socket was not created')
        sock = socket.socket(socket.AF_UNIX)
        sock.connect(str(QMP))
        sock.settimeout(0.3)
        # recv buffers complete JSON lines; a timed-out file iterator cannot be reused.
        buffer = b''
        def send(command, arguments=None, identifier=None):
            message = {'execute': command}
            if arguments is not None: message['arguments'] = arguments
            if identifier is not None: message['id'] = identifier
            try:
                sock.sendall((json.dumps(message) + '\n').encode())
                return True
            except (BrokenPipeError, ConnectionResetError):
                return False
        send('qmp_capabilities')
        last_query = 0
        while time.monotonic() < deadline and proc.poll() is None:
            observe_serial()
            now = time.monotonic()
            if now - last_query >= 1:
                if not send('query-status'):
                    break
                last_query = now
            if suspended_at is not None and not woke and now - suspended_at >= 3:
                send('system_wakeup')
                woke = True
            try:
                data = sock.recv(65536)
            except socket.timeout:
                continue
            except ConnectionResetError:
                break
            if not data:
                break
            buffer += data
            while b'\n' in buffer:
                raw, buffer = buffer.split(b'\n', 1)
                event = json.loads(raw)
                records.append(event)
                with (OUT / f'{PREFIX}-qmp-live.jsonl').open('a') as live:
                    live.write(json.dumps(event) + '\n')
                if event.get('event') == 'SHUTDOWN':
                    if event.get('data', {}).get('reason') == 'guest-reset':
                        send('human-monitor-command', {'command-line': 'info registers -a'}, 'fault-registers')
                    else:
                        send('quit')
                if event.get('id') == 'fault-registers':
                    (OUT / f'{PREFIX}-fault-registers.txt').write_text(str(event.get('return', event)))
                    send('quit')
                if event.get('event') == 'SUSPEND':
                    suspended_at = time.monotonic()
                    woke = False
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            raise RuntimeError('Guest run did not finish within the bounded test window')
    except (OSError, RuntimeError, ValueError) as error:
        failure = str(error)
    finally:
        if sock:
            sock.close()
        if proc is not None and proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                proc.wait()
        (OUT / f'{PREFIX}-qmp.json').write_text(json.dumps(records, indent=2) + '\n')
        QMP.unlink(missing_ok=True)
        QMP_DIR.cleanup()
observe_serial()
(OUT / f'{PREFIX}-clock-checks.json').write_text(json.dumps(clock_checks, indent=2) + '\n')
serial_text = (OUT / f'{PREFIX}-serial.log').read_text(errors='replace')
rounds = int(os.environ.get('S3_TEST_ROUNDS', '12'))
summary = {
    'make_exit_code': None if proc is None else proc.returncode,
    'error': failure,
    'e2e_completed': 'E2E_SUSPEND_PASS' in serial_text,
    'suspend_count': sum(e.get('event') == 'SUSPEND' for e in records),
    'wakeup_count': sum(e.get('event') == 'WAKEUP' for e in records),
    'passed_cycles': sum(f'E2E_CYCLE {cycle} PASS' in serial_text
                         for cycle in range(1, rounds + 1)),
    'clock_compensation_matches_host': (len(clock_checks) == rounds
                                          and {c['cycle'] for c in clock_checks} == set(range(1, rounds + 1))
                                          and all(c['passed'] for c in clock_checks)),
    'guest_reset': any(e.get('event') == 'SHUTDOWN'
                       and e.get('data', {}).get('reason') == 'guest-reset'
                       for e in records),
}
try:
    persisted = subprocess.run(['debugfs', '-R', 'cat pm-proof', str(OUT / 'root.img')],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               check=False, timeout=10)
    summary['disk_data_persisted'] = (persisted.returncode == 0
                                      and persisted.stdout == bytes([rounds & 255]) * 4096)
except (OSError, subprocess.TimeoutExpired) as error:
    summary['disk_data_persisted'] = False
    summary['error'] = summary['error'] or str(error)
refusal = os.environ.get('S3_TEST_UNSUPPORTED') == '1'
unsupported = re.search(r'^E2E_REBOOT_ERROR 95$', serial_text, re.M) is not None
if unsupported and not refusal:
    summary['error'] = summary['error'] or 'guest returned EOPNOTSUPP; real system suspend/resume was not completed'
checks = protocol_checks(serial_text, records, clock_checks, arch=ARCH, cpus=CPUS,
                         rounds=rounds, refusal=refusal)
summary.update(architecture=ARCH, cpus=CPUS, rounds=rounds,
               scenario='unsupported-refusal' if refusal else 'system-suspend-resume',
               unsupported=unsupported,
               checks=checks, actual_suspend_resume=(not refusal and all(checks.values())),
               launch_command=commands)
summary['passed'] = (summary['error'] is None and summary['make_exit_code'] == 0
                     and summary['disk_data_persisted'] and all(checks.values()))
(OUT / f'{PREFIX}-result.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
raise SystemExit(0 if summary['passed'] else 1)
