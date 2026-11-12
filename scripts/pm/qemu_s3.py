# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

"""Boot a userspace suspend probe and capture real QMP transitions."""
import json
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import time

OUT = Path(os.environ["S3_OUTPUT_DIR"]).resolve()
REPO = Path(__file__).resolve().parents[2]
QMP = OUT / 'qmp.sock'
QMP.unlink(missing_ok=True)
PREFIX = 'guest-e2e'
for suffix in ('result.json', 'qmp-live.jsonl'):
    (OUT / f'{PREFIX}-{suffix}').unlink(missing_ok=True)
commands = ['make', 'justrun', 'ACCEL=n', 'SMP=' + os.environ.get('S3_TEST_CPUS', '4'),
            'DISK_IMG=' + str(OUT / 'root.img'),
            'XKMAKE_ARGS=--no-net --no-vsock',
            f'QEMU_ARGS=-no-reboot -qmp unix:{QMP},server=on,wait=off']
commands[-1] += ' -no-shutdown -nic none'
if os.environ.get('S3_TEST_VGA') != '1':
    commands[-1] += ' -vga none'
if (os.environ.get('S3_TEST_UNSUPPORTED') == '1'
        and os.environ.get('S3_TEST_VGA') != '1'):
    commands[-1] += ' -global ICH9-LPC.disable_s3=1'
records = []
cycle_starts = {}
clock_checks = []
serial_lines_seen = 0
# One-second CMOS quantization plus at most two 300 ms serial polling edges.
# The remainder covers bounded marker transmission and clock sampling skew.
clock_tolerance_ns = 2_000_000_000

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

failure = None
proc = None
with (OUT / f'{PREFIX}-serial.log').open('w') as serial:
    sock = None
    try:
        proc = subprocess.Popen(commands, cwd=REPO, stdout=serial,
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
if os.environ.get('S3_TEST_UNSUPPORTED') != '1':
    try:
        persisted = subprocess.run(['debugfs', '-R', 'cat pm-proof', str(OUT / 'root.img')],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   check=False, timeout=10)
        summary['disk_data_persisted'] = (persisted.returncode == 0
                                          and persisted.stdout == bytes([rounds & 255]) * 4096)
    except (OSError, subprocess.TimeoutExpired) as error:
        summary['disk_data_persisted'] = False
        summary['error'] = summary['error'] or str(error)
else:
    summary['disk_data_persisted'] = None
summary['passed'] = (summary['error'] is None
                     and summary['make_exit_code'] == 0
                     and summary['clock_compensation_matches_host']
                     and summary['disk_data_persisted']
                     and summary['e2e_completed']
                     and not summary['guest_reset']
                     and summary['suspend_count'] == rounds
                     and summary['wakeup_count'] == rounds
                     and summary['passed_cycles'] == rounds)
if os.environ.get('S3_TEST_UNSUPPORTED') == '1':
    summary['passed'] = (summary['error'] is None
                         and summary['make_exit_code'] == 0
                         and 'E2E_REFUSAL_PASS' in serial_text
                         and summary['suspend_count'] == 0
                         and summary['wakeup_count'] == 0
                         and not summary['guest_reset'])
(OUT / f'{PREFIX}-result.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
raise SystemExit(0 if summary['passed'] else 1)
