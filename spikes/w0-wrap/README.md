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

- launcher standalone: hello frame on the wire fd, server logs + READY on
  stderr, ready-probe pass, killpg reaps the whole server tree — no leaks.
- `cargo check` clean for the Tauri spike against the real plugin.

## Compatibility matrix

| check | fastapi mac | fastapi win | fastapi linux | gradio mac | gradio win | gradio linux | streamlit mac | streamlit win | streamlit linux |
|---|---|---|---|---|---|---|---|---|---|
| page load + WS/SSE stays connected | | | | | | | | | |
| text input + long streamed output | | | | | | | | | |
| file upload dialog + drag-drop | | | | | | | | | |
| file download (save dialog, destination) | | | | | | | | | |
| mic/camera permission prompts | | | | | | | | | |
| clipboard copy/paste | | | | | | | | | |
| external link → system browser | | | | | | | | | |
| window close kills server tree | | | | | | | | | |
| kill -9 → restarting page → recover | | | | | | | | | |
| launch→ready time | | | | | | | | | |
| installer size / memory | | | | | | | | | |

Fill with pass / works-with-fix / fails + notes; results and the go/no-go
recommendation land in docs/decisions/0003-wrap-mode.md.
