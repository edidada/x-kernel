#!/usr/bin/env python3
"""Drive QEMU QMP for the Links2 framebuffer demo (screenshot + input)."""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
import time


_QMP_BUF: dict[int, bytes] = {}


def qmp_readline(sock: socket.socket, timeout: float) -> dict:
    sock.settimeout(timeout)
    key = id(sock)
    buf = _QMP_BUF.get(key, b"")
    while b"\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("QMP socket closed")
        buf += chunk
    line, buf = buf.split(b"\n", 1)
    _QMP_BUF[key] = buf
    line = line.strip()
    if not line:
        return qmp_readline(sock, timeout)
    return json.loads(line)


def qmp_connect(path: str) -> socket.socket:
    deadline = time.time() + 30
    last_err: Exception | None = None
    while time.time() < deadline:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(path)
            greeting = qmp_readline(sock, 5.0)
            if "QMP" not in greeting:
                raise RuntimeError(f"unexpected QMP greeting: {greeting}")
            return sock
        except (FileNotFoundError, ConnectionRefusedError, OSError) as err:
            last_err = err
            sock.close()
            time.sleep(0.2)
    raise RuntimeError(f"qmp connect failed: {last_err}")


def qmp_cmd(sock: socket.socket, execute: str, arguments: dict | None = None) -> dict:
    msg: dict = {"execute": execute}
    if arguments:
        msg["arguments"] = arguments
    sock.sendall((json.dumps(msg) + "\n").encode())
    deadline = time.time() + 15
    while time.time() < deadline:
        obj = qmp_readline(sock, max(1.0, deadline - time.time()))
        if "event" in obj:
            continue
        if "error" in obj:
            raise RuntimeError(f"{execute}: {obj['error']}")
        if "return" in obj:
            return obj
    raise RuntimeError(f"no QMP reply for {execute}")


def wait_log_any(log_path: str, needles: list[str], timeout: float) -> str:
    deadline = time.time() + timeout
    encoded = [n.encode() for n in needles]
    while time.time() < deadline:
        if os.path.exists(log_path):
            with open(log_path, "rb") as fh:
                data = fh.read()
            for needle, raw in zip(needles, encoded):
                if raw in data:
                    return needle
        time.sleep(0.4)
    raise TimeoutError(f"timed out waiting for {needles!r} in {log_path}")


def ppm_stats(path: str) -> dict[str, int]:
    with open(path, "rb") as fh:
        magic = fh.readline().strip()
        if magic != b"P6":
            raise RuntimeError(f"unsupported ppm {magic!r} ({path})")

        def next_token() -> bytes:
            while True:
                line = fh.readline()
                if not line:
                    raise RuntimeError("truncated ppm")
                if line.startswith(b"#"):
                    continue
                return line

        dims = next_token().split()
        width, height = int(dims[0]), int(dims[1])
        maxval = int(next_token())
        raw = fh.read()
    pixels = width * height
    if maxval != 255 or len(raw) < pixels * 3:
        raise RuntimeError(f"ppm payload mismatch {len(raw)} vs {pixels * 3}")
    black = magenta = light = other = 0
    for i in range(0, pixels * 3, 3):
        r, g, b = raw[i], raw[i + 1], raw[i + 2]
        if r == 0 and g == 0 and b == 0:
            black += 1
        elif r == 0xFF and g == 0x00 and b == 0xFF:
            magenta += 1
        elif r >= 180 and g >= 180 and b >= 180:
            light += 1
        else:
            other += 1
    return {
        "width": width,
        "height": height,
        "black": black,
        "magenta": magenta,
        "light": light,
        "other": other,
        "painted": light + other,
    }


def send_rel(sock: socket.socket, dx: int, dy: int) -> None:
    qmp_cmd(
        sock,
        "input-send-event",
        {
            "events": [
                {"type": "rel", "data": {"axis": "x", "value": dx}},
                {"type": "rel", "data": {"axis": "y", "value": dy}},
            ]
        },
    )


def send_btn(sock: socket.socket, button: str, down: bool) -> None:
    qmp_cmd(
        sock,
        "input-send-event",
        {"events": [{"type": "btn", "data": {"down": down, "button": button}}]},
    )


def send_key(sock: socket.socket, qcode: str, device: str | None = None) -> None:
    arguments: dict = {
        "events": [
            {
                "type": "key",
                "data": {
                    "down": True,
                    "key": {"type": "qcode", "data": qcode},
                },
            }
        ]
    }
    if device:
        arguments["device"] = device
    qmp_cmd(sock, "input-send-event", arguments)
    arguments["events"][0]["data"]["down"] = False
    qmp_cmd(sock, "input-send-event", arguments)


def hmp_sendkey(sock: socket.socket, keys: str) -> None:
    qmp_cmd(
        sock,
        "human-monitor-command",
        {"command-line": f"sendkey {keys}"},
    )


def find_keyboard_device(sock: socket.socket) -> str | None:
    try:
        pci = qmp_cmd(sock, "query-pci")["return"]
    except RuntimeError:
        return None
    for bus in pci:
        for dev in bus.get("devices", []):
            qdev_id = dev.get("qdev_id") or ""
            desc = json.dumps(dev)
            if "keyboard" in desc.lower() or "keyboard" in qdev_id.lower():
                if qdev_id:
                    return qdev_id
    return None


def ppm_diff_bytes(a: str, b: str) -> int:
    def payload(path: str) -> bytes:
        with open(path, "rb") as fh:
            data = fh.read()
        return data.split(b"\n", 3)[3]

    pa, pb = payload(a), payload(b)
    return sum(1 for x, y in zip(pa, pb) if x != y)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--qmp", required=True)
    parser.add_argument("--log", required=True)
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--wait-for", default="cjk dump ok")
    parser.add_argument("--settle-s", type=float, default=8.0)
    parser.add_argument("--kbd-probe", action="store_true")
    args = parser.parse_args()
    os.makedirs(args.out_dir, exist_ok=True)

    sock = None
    kbd_dev = None
    if args.kbd_probe:
        wait_log_any(args.log, ["kbd-probe: waiting"], timeout=80)
        sock = qmp_connect(args.qmp)
        qmp_cmd(sock, "qmp_capabilities")
        kbd_dev = find_keyboard_device(sock)
        print(f"qmp-e2e: keyboard qdev_id={kbd_dev!r}")
        for qcode in ("g", "o"):
            try:
                send_key(sock, qcode, kbd_dev)
            except RuntimeError as err:
                print(f"qmp-e2e: input-send-event {qcode} failed: {err}")
                hmp_sendkey(sock, qcode)
        kbd_result = wait_log_any(
            args.log, ["kbd-probe: got", "kbd-probe: no bytes"], timeout=12
        )
        print(f"qmp-e2e: {kbd_result}")

    wait_log_any(args.log, [args.wait_for], timeout=110)
    time.sleep(args.settle_s)

    if sock is None:
        sock = qmp_connect(args.qmp)
        qmp_cmd(sock, "qmp_capabilities")
        kbd_dev = find_keyboard_device(sock)
        print(f"qmp-e2e: keyboard qdev_id={kbd_dev!r}")

    page = os.path.join(args.out_dir, "links2-example.ppm")
    mouse = os.path.join(args.out_dir, "links2-mouse.ppm")
    keys = os.path.join(args.out_dir, "links2-keys.ppm")

    def dump(path: str) -> None:
        qmp_cmd(sock, "screendump", {"filename": os.path.abspath(path)})

    dump(page)
    send_rel(sock, 80, 40)
    send_btn(sock, "left", True)
    send_btn(sock, "left", False)
    send_btn(sock, "right", True)
    send_btn(sock, "right", False)
    send_btn(sock, "middle", True)
    send_btn(sock, "middle", False)
    try:
        send_btn(sock, "wheel-down", True)
        send_btn(sock, "wheel-down", False)
    except RuntimeError as err:
        print(f"qmp-e2e: wheel button skipped: {err}")
    time.sleep(1.0)
    dump(mouse)
    for qcode in ("g", "tab", "ret", "esc"):
        try:
            send_key(sock, qcode, kbd_dev)
        except RuntimeError:
            hmp_sendkey(sock, qcode)
        time.sleep(0.2)
    time.sleep(0.8)
    dump(keys)

    stats = ppm_stats(page)
    mouse_diff = ppm_diff_bytes(page, mouse)
    key_diff = ppm_diff_bytes(mouse, keys)
    print(f"qmp-e2e page: {stats}")
    print(f"qmp-e2e mouse_diff_bytes={mouse_diff} key_diff_bytes={key_diff}")
    pixels = stats["width"] * stats["height"]
    if stats["light"] < 1000 and stats["painted"] < pixels // 16:
        print("qmp-e2e: screendump does not look painted", file=sys.stderr)
        return 1
    if mouse_diff == 0:
        print("qmp-e2e: mouse events did not change the scanout", file=sys.stderr)
        return 1
    print("qmp-e2e: screendump + input-send-event ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
