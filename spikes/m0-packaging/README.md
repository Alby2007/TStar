# M0 packaging spike

Throwaway Tauri app validating the riskiest assumption in the project:
**a relocatable python-build-standalone interpreter + a uv-locked package
dir can be bundled as app resources and run on a machine with no Python.**

Findings and numbers: [`docs/decisions/0001-python-packaging.md`](../../docs/decisions/0001-python-packaging.md).

What it does:

- `scripts/fetch-python.mjs` downloads a pinned python-build-standalone
  release (`cpython-3.12.16+20261110`) for the host (or `--target`) triple
  and unpacks it into `src-tauri/resources/python`.
- `scripts/prepare-env.mjs` installs `backend/requirements.txt` (locked via
  `uv pip compile`) into `src-tauri/resources/site-packages` **using the
  bundled interpreter itself**, so wheels always match the target — and
  syncs `backend/` into the staged tree so `src-tauri/resources/` mirrors
  the installed app byte for byte.
- `backend/app.py` serves a length-prefixed JSON frame protocol on stdio
  (throwaway — not the M1 design) and supports `--self-test` for
  verification in environments with no frame client.
- Rust (`src-tauri/src/main.rs`) exposes `ping_python`: spawn → write frame
  → read frame → return parsed JSON. Spawn-per-call on purpose; the
  supervisor is M1.
- The webview calls `ping_python` and renders the response (Python version,
  executable path, numpy version + a computed sum — proof a native wheel
  imported fine inside the bundle).
- `scripts/verify-bundled.{sh,ps1}` run the bundled payload's `--self-test`
  against a staged or installed resource tree and assert the interpreter
  path + numpy import — this is the clean-machine check CI runs.

## Run

```sh
pnpm install
pnpm compile:reqs        # regenerate backend/requirements.txt (uv)
pnpm stage               # fetch interpreter + build site-packages + backend
pnpm tauri dev           # dev mode
pnpm tauri build         # installer(s) under src-tauri/target/release/bundle/
./scripts/verify-bundled.sh src-tauri/resources   # payload check
```

If the installed app shows the ping response — including
`"executable": ".../resources/python/..."` — the spike's premise holds.
