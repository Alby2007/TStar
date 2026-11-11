#!/usr/bin/env python3
"""Spike check: launcher stdin-EOF -> kill_tree -> a SIGTERM-ignoring
grandchild must still die (group/taskkill-T escalation, not leader-only).

Cross-platform: posix uses pgrep on the launcher group; Windows relies on
taskkill /T. Runs on CI runners with just python3 (stdlib only).
"""

import os
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ORPHAN = os.path.join(tempfile.gettempdir(), "w0-orphan.txt")
PY = sys.executable

if os.path.exists(ORPHAN):
    os.remove(ORPHAN)

env = {**os.environ, "W0_ORPHAN_FILE": ORPHAN}
kwargs = {}
if os.name == "posix":
    kwargs["preexec_fn"] = os.setsid
else:
    kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)

launcher = subprocess.Popen(
    [PY, "-u", "launcher.py", "--", PY, "apps/orphan_server.py"],
    cwd=HERE,
    env=env,
    stdin=subprocess.PIPE,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.PIPE,
    **kwargs,
)

ready = False
t0 = time.time()
while time.time() - t0 < 30:
    line = launcher.stderr.readline().decode(errors="replace")
    sys.stderr.write(line)
    if line.startswith("READY "):
        ready = True
        break
assert ready, "server never became ready"

# Let the orphan start writing its heartbeat.
time.sleep(1.0)
first = os.path.getsize(ORPHAN) if os.path.exists(ORPHAN) else 0
assert first > 0, "orphan never started"

# stdin EOF — same as the supervisor closing the pipe.
launcher.stdin.close()
rc = launcher.wait(timeout=15)
time.sleep(4.5)  # past the 3s SIGKILL grace

grown = os.path.exists(ORPHAN) and os.path.getsize(ORPHAN) > first + 100

survivors = []
if os.name == "posix":
    out = subprocess.run(
        ["pgrep", "-g", str(launcher.pid)], capture_output=True, text=True
    ).stdout
    survivors = [p for p in out.split() if p.strip()]

print(
    f"launcher exit {rc}; group members left: {survivors}; "
    f"orphan file growing: {grown}"
)
assert not survivors, f"leaked group members: {survivors}"
assert not grown, "orphan grandchild survived kill_tree"
print("PASS: kill_tree reaped the whole tree incl. SIGTERM-ignoring grandchild")
