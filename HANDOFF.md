# Handoff — RC.6 CI fix verification (mid-flight)

**State at handoff:** commit `2e9b483` + the working-tree fixes committed with
this document. Everything below reflects what was true when this was written.

## What this commit fixes (CI run #90 failures)

Three real failures, all diagnosed, all fixed in this working tree:

1. **Rust/Ubuntu compile error** — `tokio::process::Child::from_std` does not
   exist. `crates/tauri-plugin-pylair/src/supervisor.rs`
   `spawn_via_pdeathsig_anchor()` was rewritten: the anchor thread now carries
   the tokio `Handle` in, calls `rt.enter()`, then `TokioCommand::spawn()` —
   the fork still happens ON the parked anchor thread (the pdeathsig
   guarantee), and `kill_on_drop(true)` is restored.
2. **Ruff ×3** — `python/pylair/src/pylair/_worker.py:52` broad
   `except Exception: pass` narrowed to `(ImportError, OSError)` with a
   stderr diagnostic; `python/pylair/src/pylair/_tasks.py` keeps its
   deliberate `except BaseException` catches with `noqa: BLE001` reasons.
   `ruff check` is GREEN locally.
3. `python/pylair/uv.lock` — version drift rc2→rc3, belongs in this batch.

## Proven / not proven

- **Green locally (macOS):** `cargo check --lib`, `cargo fmt --check`,
  `ruff check`, 50/50 pytest.
- **NOT proven:** the Linux build. An `ubuntu:24.04` Docker verification was
  RUNNING when handed off — all deps resolved, `tauri` compiled, and the
  plugin itself was mid-clippy when the container was stopped. No Linux
  verdict exists. The 35s pdeathsig test and the abrupt-kill tests have
  NEVER run on Linux.
- macOS wedged-runtime orphan remains a documented gap (README Known
  Limitations).

## Next step — run Linux verification (the script that was running)

```bash
cat > /tmp/pylair-linux-verify.sh <<'EOF'
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential curl pkg-config python3 \
  libglib2.0-dev libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libdbus-1-dev > /dev/null
curl --proto '=https' -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
export CARGO_TARGET_DIR=/tmp/pylair-target   # do NOT share the host's target/
cd /ws
rustc --version && python3 --version
cargo clippy -p tauri-plugin-pylair --all-targets -- -D warnings
cargo test -p tauri-plugin-pylair
curl -LsSf https://astral.sh/ruff/install.sh | sh > /dev/null
export PATH="$HOME/.local/bin:$PATH"
cd python/pylair && ruff check src/pylair/ tests/
echo "=== LINUX VERIFY COMPLETE ==="
EOF
docker run --rm -i -v "$PWD":/ws -w /ws ubuntu:24.04 bash /dev/stdin \
  < /tmp/pylair-linux-verify.sh
```

Notes: `ubuntu:24.04` matches `ubuntu-latest` CI and ships Python 3.12. The
repo's `rust-toolchain.toml` auto-installs the pinned toolchain inside the
container (it picked 1.92.0, ignoring the preinstalled one — expected). `-i`
is required for `/dev/stdin` to work. First run takes ~20–30 min (apt webkit
stack + full tauri dep build); target dir is container-local.

## Then

- If green: push, watch CI on the new commit — Ubuntu rust + both Ubuntu
  acceptance legs + macOS/Ubuntu ruff should all clear.
- Report Linux results as Linux results — do not extend macOS test claims.
- **Do not publish npm rc.6 until CI is green.**

## Release order (after CI green) — user-corrected, do not reorder

1. crates.io: `tauri-plugin-pylair` `0.1.0-rc.3`
2. PyPI: `pylair` `0.1.0-rc.3`
3. Regenerate `packages/create-pylair/templates/default/src-tauri/Cargo.lock`
   against the published crate, commit it (the lock ships verbatim in the
   npm tarball; scaffold a registry-mode app, resolve, copy the lock back)
4. Pack-mode CI green on THAT commit
5. npm trio `0.1.0-rc.6`
6. Registry acceptance `source=registry`

## Known flags for release notes

- `cancel(): boolean → CancelOutcome` is a BREAKING client change —
  `if (await cancel(t))` callers must update. Template has no cancel usage
  (verified).
- The template `Cargo.lock` is still at crate rc.2 — expected until step 3.
- Prior context: full summary at
  `~/.local/share/devin/cli/summaries/history_47b24b3a7d554c5b.md` (does not
  travel — key facts duplicated here).
