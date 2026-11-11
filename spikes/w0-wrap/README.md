# W0 wrap spike

Throwaway validation for wrap mode (docs/wrap-mode-brief.md §4). Two
questions: do real Gradio/Streamlit/command apps behave acceptably in the
three native webviews, and does the existing supervisor model work for a
long-starting HTTP server?

## Layout

- `launcher.py` — hand-wired stand-in for `python -m pylair.wrap`. Speaks
  the supervisor frame protocol on a private fd (hello immediately,
  pongs, clean shutdown, non-zero exit on server death), redirects its
  own fd 1 to stderr for hygiene, picks a free loopback port, exports
  `PYLAIR_PORT`, substitutes `{port}` in the command, polls `GET /`
  until non-5xx, then emits `READY <url>` on stderr (the supervisor
  forwards stderr lines as events — no protocol change needed).
- `apps/fastapi_app.py` — command-mode sample (marker, upload, download,
  SSE stream, external link).
- `apps/gradio_app.py` — chat + file up/download + image (calls launch()
  directly; adapters are W3).
- `apps/streamlit_app.py` — widgets + uploader + download_button.
- `src-tauri/` — minimal Tauri app: window loads `public/index.html`
  ("starting"), spawns the launcher under the real supervisor, and calls
  `webview.navigate()` to the emitted `READY` url.

## Run

    uv venv .venv && uv pip install --python .venv/bin/python fastapi uvicorn python-multipart
    pnpm install
    pnpm dev:fastapi      # or dev:gradio / dev:streamlit with those deps

## Verified so far (macOS, arm64)

Deps pinned: **gradio 5.49.1, streamlit 1.51.0** (+ fastapi/uvicorn/
python-multipart) in the spike-local `.venv`.

- launcher standalone: hello frame on the wire fd (saved before the fd-1
  redirect), server logs + `READY <url>` on stderr, ready-probe pass.
- Hung-server proof (the W1 HTTP-probe rationale): server answers the
  first request then holds connections open forever — pongs still pass
  (they come from the launcher, not the server), but 3 failed health
  probes → launcher exits 5 for supervisor restart.
- `kill_tree`: whole process group reaped including a SIGTERM-ignoring
  grandchild (`test_kill_tree.py`) — escalation enumerates group
  members, not just the leader.
- Design finding: the server must share the launcher's process group
  (no setsid) or a supervisor-side killpg can't reach it.
- `os._exit` after kill_tree — daemon stdin reader holds the buffer lock
  at shutdown; sys.exit deadlocks (all exit paths after the reader
  thread starts use it).
- `cargo check` clean for the Tauri spike against the real plugin.
- IPC-leak probe wired into the fastapi page (page POSTs the Tauri
  surface it sees to /ipc-probe → stderr + /tmp/w0-ipc.json); real
  webview run still pending — see CHECKLIST.md.

## macOS real-webview runs (arm64, tauri dev)

All three apps launched under the real supervisor and navigated to their
loopback URL. Findings that matter beyond the spike:

- **Plugin bug found and fixed** (crates commit): `supervisor::spawn`
  used bare `tokio::spawn` inside plugin `setup`, which runs on the main
  thread with no reactor entered — every app panicked at launch
  ("there is no reactor running"). `spawn` now returns
  `(Handle, actor)` and the caller drives the future via
  `tauri::async_runtime`. Scaffolded apps would have hit the same panic.
- **Event payloads arrive JSON-quoted**: `emit(event, line)` serializes
  the String, so `Event::payload()` is `"READY ..."` with literal
  quotes. Strip or deserialize before matching (spike fix).
- **IPC probe — hard pass.** From the remote page:
  `__TAURI__` undefined; `__TAURI_INTERNALS__.invoke` exists but every
  attempt is REJECTED — `plugin:event|emit`: "not allowed on ... URL:
  http://127.0.0.1:PORT/ (allowed on: URL: local)"; `python_call`,
  `python_shutdown` and a bogus baseline: "pylair.X not allowed.
  Plugin not found". Remote origins get zero usable IPC; capabilities
  only ever scope to local URLs. Recorded in /tmp/w0-ipc.json.
- **kill -9 the server** → `launcher: server exited` → supervisor
  restart → new port → READY → window re-navigates. Full cycle seconds.
- **App quit (SIGTERM/quit event)** → stdin EOF → launcher kill_tree →
  zero survivors (verified fastapi + streamlit).
- **SSE** (fastapi `/stream`): EventSource connection held, ticks
  rendered.
- **Streamlit**: `_stcore` websocket transport live (6+ established
  conns), zero health-probe misses.
- **Gradio**: page loads, multi-connection session live. One
  health-probe miss during first paint (recovered, never reached 3) —
  W1 should probe a cheap path or tolerate a slow first minute.
- Launch→ready ≈ 1-3s per app after the one-time cargo build.

## Compatibility matrix

| check | fastapi mac | fastapi win | fastapi linux | gradio mac | gradio win | gradio linux | streamlit mac | streamlit win | streamlit linux |
|---|---|---|---|---|---|---|---|---|---|
| page load + WS/SSE stays connected | pass (SSE held) | | | pass (chat works) | | | pass (ws conns) | | |
| text input + long streamed output | pass (SSE ticks) | | | pass (chat echo) | | | pass (rerun) | | |
| file upload dialog + drag-drop | pass (native picker) | | | pass (native picker) | | | pass (native picker) | | |
| file download (save dialog, destination) | quirk¹ | | | quirk¹ | | | silent² | | |
| mic/camera permission prompts | n/a in sample | | | n/a in sample | | | n/a | | |
| clipboard copy/paste | pass | | | pass | | | pass | | |
| external link → system browser | fail³ | | | fail³ | | | fail³ | | |
| window close kills server tree | pass (0 survivors) | | | pass | | | pass (0 survivors) | | |
| kill -9 → restarting page → recover | pass (re-nav, 3x) | | | pass (re-nav) | | | pass⁴ | | |
| IPC from remote page | pass (all REJECTED) | | | (same origin policy) | | | (same) | | |
| launch→ready time | ~1-3s | | | ~2s | | | ~2s | | |
| installer size / memory | | | | | | | | | |

¹ download: file lands in ~/Downloads silently (no save dialog) AND the
  webview navigates to display the content — user stranded with no
  back/reload. W1: register a download handler + keep navigation on the
  app origin.
² streamlit download_button: downloads silently to ~/Downloads with ZERO
  feedback — no dialog, no toast. Mechanically fine, UX trap.
³ external link (target=_blank / link_button): silently swallowed —
  wry drops new-window requests without a registered
  new_window_req_handler. W1: handler → shell.open in system browser,
  plus navigation_handler blocking off-origin in-place nav.
⁴ streamlit kill -9: server killed → launcher respawned on a new port →
  the page simply reloaded, no error/reconnect UI shown. State is LOST:
  the filled name input came back empty — Streamlit sessions are
  in-memory, so every restart is a fresh session. Users see a page
  reload that silently forgets their work.
Also noted: a full-page navigation to an error/JSON response strands
the user — no back button, Cmd+R not wired.

Fill with pass / works-with-fix / fails + notes; results and the go/no-go
recommendation land in docs/decisions/0003-wrap-mode.md.
