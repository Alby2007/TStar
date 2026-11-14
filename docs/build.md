# pylair build

`pylair build` turns a pylair app into a signed or unsigned native installer
with Python bundled inside. It is the M0 packaging recipe made generic —
see `docs/decisions/0001-python-packaging.md` for why
python-build-standalone + `uv pip install --target` was chosen.

## Pipeline

```
pyproject.toml [tool.pylair] + src-tauri/tauri.conf.json
        │
        ▼
1. fetch python-build-standalone CPython for the HOST triple
   (SHA256-verified against hashes committed in the CLI — a checksum
   fetched from the same release only catches corruption; tarball
   cached in .pylair/cache/ between builds)
2. uv export --frozen from uv.lock → uv pip install --target
   src-tauri/resources/site-packages using the bundled interpreter —
   the SAME locked, hashed set `pylair dev` syncs (requirements.txt is
   the no-lockfile fallback, with a drift warning)
3. copy the app source dir into src-tauri/resources/
4. prune the staged tree (default deny-list + app additions)
5. import-check the app module with the staged tree
6. npx tauri build
7. warn loudly if no signing credentials are configured
```

## Network access on first build

A first build needs GitHub access twice: we download
python-build-standalone (step 1, then cached in `.pylair/cache/`), and
Tauri's bundler downloads its installer toolchain — WiX and NSIS on
Windows, the AppImage runtime on Linux — on first use. Later builds
reuse the caches; an offline rebuild works once they've been warmed.

## Configuration — one file

Everything pylair-specific lives in `[tool.pylair]` in `pyproject.toml`.
App identity (`productName`, `identifier`, `version`) stays in
`src-tauri/tauri.conf.json` — Tauri owns it, we derive from it. No third
config file.

```toml
[tool.pylair]
app-module = "app.backend"        # required: module holding @expose functions
source = "backend"                # dir staged into resources/ (default "backend")
requirements = "requirements.txt" # locked deps (default "requirements.txt")
python-version = "3.12"           # bundled CPython class: "3.12" | "3.13"
prune = ["my/extra/dir"]          # additions to the default deny-list
bundles = "dmg,app"               # optional override for tauri --bundles
include = [".env.example"]        # stage a path even though it matches
                                  # the secret-looking exclusions
```

## What ships from your source folder

The build copies `source` (default `backend/`) into the installer
verbatim — minus anything that looks like a secret. Skipped by default:

- any path segment starting with `.` — `.env`, `.env.*`, `.git`,
  `.venv`, `.DS_Store`, `.hidden/…`;
- basenames matching `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`,
  `id_ed25519*`, `credentials*.json`, `service-account*.json`.

Skips are a warning, not an error: the build prints how many paths were
refused and the first ten names. To ship one deliberately, list it in
`include = [...]` — globs relative to `source`, same shape as `prune`
entries; an included path is staged even inside a refused directory.

Do not put secrets in the backend folder. The exclusion list is a
seatbelt for accidents, not a secret store — a committed `.env` is
already in your git history even if the installer drops it. Ship
configuration through the user's data dir at runtime
(`app.path().app_data_dir()` / `path_provider`), written on first run
or entered in a settings window.

## Pruning

The standalone CPython distribution ships things an installed app never
needs; site-packages does too. `pylair build` deletes them before bundling.
Default deny-list (globs relative to `src-tauri/resources/`):

| Pattern | Why it's dead weight |
|---|---|
| `python/include`, `*.a`, `lib/pkgconfig` | C headers / static libs — build-time only |
| `python/lib/python*/test`, `idlelib`, `tkinter`, `turtledemo` | stdlib test suite, IDLE, tk |
| `pydoc_data`, `ensurepip`, `venv`, `share` | docs topics, pip bootstrap, man pages |
| `site-packages/pip*`, `_distutils_hack` | pip is dead weight post-install |
| `site-packages/*/tests`, `*/test` | per-package test suites (numpy's is ~25 MB) |
| `**/*.pyc`, `**/__pycache__` | regenerated caches |

Deliberately **not** pruned:
- `*/testing` — `numpy.testing`, `pandas.testing`, `torch.testing` are
  imported at runtime by downstream code.
- `pkg_resources` / `setuptools` — still imported at runtime by many libs.
- `LICENSE.txt` and other license files — the PSF license *requires* them
  to ship with redistributed interpreters.

After pruning, the build imports the app module with the staged tree
(`PYTHONNOUSERSITE=1`) and fails if it breaks — over-pruning shows up at
build time, not on the user's machine.

Apps extend it via `prune = [...]`. If an app genuinely ships a package
named `tests` or needs pip at runtime (e.g. a plugin installer), that entry
belongs in a future `prune-exclude` — for now, deny-list is deny-list.

## No cross-compiling

Pylair bundles *native* wheels. A Linux host cannot produce a Windows
`.pyd`, and Tauri itself does not cross-compile. `pylair build` therefore:

- always fetches the interpreter for the host triple and runs `tauri build`
  without `--target`;
- errors early if the host `platform:arch` has no standalone CPython build
  (`wasm32`, 32-bit, etc.);
- should be invoked once per target OS — in CI, that's a matrix job.

Cross-OS installers are out of scope by design, matching Tauri's own model.

## Signing

**Unsigned by default.** `tauri build` produces an unsigned bundle and
pylair prints a loud warning — unsigned apps trigger Gatekeeper on macOS
and SmartScreen on Windows, which is correct and intended: users should
see that warning until real signing is configured.

On macOS, `pylair build` still re-signs the bundle **ad-hoc** when no
identity is configured — every Mach-O binary inside the `.app` (the
vendored CPython tree included), verified with
`codesign --verify --deep --strict` before the build returns. Tauri's
own output leaves only a linker stub signature on the main binary,
which is the worse state for Gatekeeper. Ad-hoc signing does **not**
remove the warning — a downloaded `.app` still needs
right-click → **Open**, or `xattr -dr com.apple.quarantine <app>` — it
makes the signature self-consistent so the verdict is a plain warning,
not an "is damaged" refusal.

Signing credentials are read from environment variables, never config
files (secrets don't belong in VCS):

| Env var | Platform | Purpose |
|---|---|---|
| `APPLE_SIGNING_IDENTITY` | macOS | Developer ID Application cert |
| `APPLE_CERTIFICATE` + `APPLE_CERTIFICATE_PASSWORD` | macOS | base64 p12 for CI |
| `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID` or `APPLE_API_KEY`/`APPLE_API_ISSUER`/`APPLE_API_KEY_PATH` | macOS | notarization |
| `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | all | **updater** signatures only — does NOT sign the app and does not silence the unsigned warning |
| `WINDOWS_CERTIFICATE_THUMBPRINT` | Windows | cert in the machine store |
| `AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET` + `AZURE_CODE_SIGNING_*` | Windows | Azure Trusted Signing (CI-friendly, no dongle) |

Open item: macOS **notarization of an embedded Python** — every `.so` and
`.dylib` inside `resources/` must be individually signed under the hardened
runtime before the outer bundle is notarized. This is where projects like
this usually get stuck, so it is tracked as its own milestone (M5/M6
risk item), not folded into the first build.
