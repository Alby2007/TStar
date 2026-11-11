# W0 manual checklist — ~15 min per OS

Run each app once per OS. Record pass / fix / fail + notes into the matrix
in README.md; versions under test: **gradio 5.49.1, streamlit 1.51.0,
fastapi (latest)**, launcher.py rev in this commit.

Setup per OS:

    git clone <repo> && cd spikes/w0-wrap
    uv venv .venv
    uv pip install --python .venv/bin/python \
      "gradio==5.49.1" "streamlit==1.51.0" fastapi uvicorn python-multipart
    pnpm install            # @tauri-apps/cli only
    pnpm dev:fastapi        # then repeat with dev:gradio, dev:streamlit

Windows note: use `uv pip install --python .venv\Scripts\python.exe ...`.
The launcher uses taskkill /T for tree kill — the orphan row matters most
on Windows (Job Objects arrive in W1; taskkill is the spike stand-in).

## Per app (fastapi, gradio, streamlit)

1. **Load** — window shows "starting" then the app page within ~seconds.
   Note launch→ready time from the terminal output.
2. **Interaction** — type text / move a widget; output appears (gradio
   chat echo, streamlit widget write, fastapi form).
3. **Streaming** — fastapi: SSE ticks appear in the page; gradio: chat
   response streams; streamlit: widget updates round-trip.
4. **Upload** — file picker opens, select a file, app reports its name
   + size. Try drag-and-drop too.
5. **Download** — click download; note whether a save dialog appears and
   where the file lands (this is a kill-criterion row).
6. **External link** — click it: must open the system browser, NOT a new
   webview or in-place navigation.
7. **Clipboard** — copy text out of the page, paste somewhere else.
8. **Mic/camera** (gradio only) — if the demo exposes it, allow the
   prompt; macOS: does the bundle ask permission properly or crash?
9. **Kill test** — in another terminal: `kill -9 <server pid>`
   (Windows: `taskkill /F /PID`). Window must not sit on a dead page —
   supervisor restarts; note the state shown and recovery time.
10. **Close test** — close the window; `pgrep -f apps/` must be empty
    within a few seconds (Windows: check Task Manager / `tasklist`).

## Automated rows already verified (macOS)

- launcher frame/stderr split, READY emission — pass
- hung-server probe: 3 failed health checks → exit 5 for restart — pass
- kill_tree: whole process group reaped incl. SIGTERM-ignoring grandchild
  (test_kill_tree.py) — pass
- IPC probe wired (page POSTs window.__TAURI__/__TAURI_INTERNALS__ types
  to /ipc-probe → stderr + /tmp/w0-ipc.json) — awaiting real webview run
