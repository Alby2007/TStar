#!/usr/bin/env python3
"""Spike check: launcher stdin-EOF → kill_tree → SIGTERM-ignoring
grandchild must still die (group escalation, not leader-only)."""

import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ORPHAN = "/tmp/w0-orphan.txt"
PY = os.path.join(HERE, ".venv", "bin", "python")

if os.path.exists(ORPHAN):
    os.remove(ORPHAN)

# Own pgid like the supervisor gives its child.
launcher = subprocess.Popen(
    [PY, "-u", "launcher.py", "--", PY, "apps/orphan_server.py"],
    cwd=HERE,
    stdin=subprocess.PIPE,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    preexec_fn=os.setsid,
)

ready = False
t0 = time.time()
while time.time() - t0 < 30:
    line = launcher.stderr.readline().decode()
    sys.stderr.write(line)
    if line.startswith("READY "):
        ready = True
        break
assert ready, "server never became ready"

# Let the orphan start writing its heartbeat.
time.sleep(1.0)
first = os.path.getsize(ORPHAN) if os.path.exists(ORPHAN) else 0

# stdin EOF — same as the supervisor closing the pipe.
launcher.stdin.close()
rc = launcher.wait(timeout=10)
time.sleep(4.5)  # past the 3s SIGKILL grace

survivors = []
for pid in subprocess.run(["pgrep", "-g", str(launcher.pid)], capture_output=True, text=True).stdout.split():
    survivors.append(pid)

grown = os.path.exists(ORPHAN) and os.path.getsize(ORPHAN) > first + 100
print(f"launcher exit {rc}; group members left: {survivors}; orphan file growing: {grown}")
assert not survivors, f"leaked group members: {survivors}"
assert not grown, "orphan grandchild survived"
print("PASS: kill_tree reaped the whole group incl. SIGTERM-ignoring grandchild")
