#!/usr/bin/env python3
"""W0 hand-wired wrap launcher — stands in for `python -m pylair.wrap`.

Speaks the supervisor frame protocol over stdio (hello immediately, then
a tagged stderr line `READY <url>` once the HTTP probe passes — protocol
`ready` is W1; this spike needs no supervisor changes).

Usage: launcher.py -- cmd...   ({port} in cmd is substituted; PYLAIR_PORT
is also exported for the child)
"""

import json
import os
import signal
import socket
import struct
import subprocess
import sys
import time
import urllib.request

READY_TIMEOUT = 120.0

# Private frame fd: save the real stdout BEFORE redirecting fd 1 to
# stderr, exactly like the production runtime does.
FRAME_FD = os.dup(1)
_frames = os.fdopen(FRAME_FD, "wb")


def send(obj):
    b = json.dumps(obj).encode()
    _frames.write(struct.pack(">I", len(b)) + b)
    _frames.flush()


def note(line):  # stderr is drained and forwarded by the supervisor
    sys.stderr.write(line + "\n")
    sys.stderr.flush()


def free_port():
    for _ in range(5):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]
        except OSError:
            continue
        finally:
            s.close()
    raise RuntimeError("no free loopback port")


def wait_ready(url, proc, timeout=READY_TIMEOUT):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            return False
        try:
            code = urllib.request.urlopen(url, timeout=1).status
            if code < 500:
                return True
        except Exception:
            pass
        time.sleep(0.2)
    return False


def main():
    argv = sys.argv[1:]
    if argv and argv[0] == "--":
        argv = argv[1:]
    if not argv:
        note("launcher: no command")
        sys.exit(2)

    # fd-1 hygiene: user/server stdout noise goes to stderr, never fd 1.
    # stdin reads keep using sys.stdin.buffer (fd 0 is untouched).
    os.dup2(2, 1)
    send({"jsonrpc": "2.0", "method": "hello", "params": {"protocol": 1, "exposes": [], "kind": "wrap"}})

    port = free_port()
    cmd = [a.replace("{port}", str(port)) for a in argv]
    env = {**os.environ, "PYLAIR_PORT": str(port)}
    if os.name == "posix":
        proc = subprocess.Popen(cmd, env=env, preexec_fn=os.setsid, stderr=None)
    else:
        proc = subprocess.Popen(cmd, env=env, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    url = f"http://127.0.0.1:{port}"
    note(f"launcher: spawning {' '.join(cmd)} on {url}")

    ready = wait_ready(url + "/", proc)
    note(("READY " if ready else "READY-FAIL ") + url)
    if not ready:
        kill_tree(proc)
        sys.exit(3)

    while True:
        h = sys.stdin.buffer.read(4)
        if len(h) < 4:
            break  # host closed stdin — clean exit
        (n,) = struct.unpack(">I", h)
        m = json.loads(sys.stdin.buffer.read(n))
        if m.get("method") == "ping":
            send({"jsonrpc": "2.0", "method": "pong", "params": {"seq": m["params"]["seq"]}})
        elif m.get("method") == "shutdown":
            send({"jsonrpc": "2.0", "id": m["id"], "result": None})
            break
        if proc.poll() is not None:
            note("launcher: server exited")
            sys.exit(1)
    kill_tree(proc)


def kill_tree(proc):
    try:
        if os.name == "posix":
            os.killpg(proc.pid, signal.SIGTERM)
            time.sleep(0.5)
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)
        else:
            proc.terminate()
    except ProcessLookupError:
        pass


if __name__ == "__main__":
    main()
