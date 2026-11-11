# Pylair (working title)

Desktop framework for Python and local AI applications: run one command,
get a Tauri desktop app with a modern web front end and a managed,
bundled Python runtime — no Python install required for end users.

See [PROJECT_BRIEF.md](PROJECT_BRIEF.md) for goals, architecture and
milestones. The name is a placeholder; naming lives in `product.json`.

## Status

Release candidate. `pylair` and `pylair-ollama` are on PyPI,
`tauri-plugin-pylair` is on crates.io; npm packages follow.

Install RCs with **exact pins**, not `--pre`:

```sh
pip install pylair==0.1.0rc1 pylair-ollama==0.1.0rc2
```

`--pre` opts *every* package in the install into prereleases — it can
pull your dependencies into unstable versions and break for reasons
unrelated to this project. Exact pins avoid that.

Each package versions independently across registries — the numbers
won't match (`pylair` and the crate are at `0.1.0-rc.1`, `pylair-ollama`
at `0.1.0-rc.2`). Check each registry for its latest RC.

Milestones: `M0` packaging spike → `M1` supervisor/protocol → `M2` typed
bridge → `M3` streaming/cancellation → `M4` CLI/scaffolder → `M5` Ollama
integration → `M6` reference app + RC.

## Layout

| Path | What |
|---|---|
| `crates/tauri-plugin-pylair` | Rust plugin: supervisor, IPC, permissions |
| `packages/client` | `@pylair/client` — typed calls, streams, cancellation |
| `packages/cli` | `pylair` CLI — dev, build, doctor, codegen |
| `packages/create-pylair` | project scaffolder |
| `python/pylair` | Python runtime — `@expose`, protocol server, tasks |
| `integrations/` | optional: ollama (M5), fastapi (later) |
| `templates/` | scaffold templates (not in pnpm workspace, by design) |
| `examples/` | `reference-app` — the real acceptance test (M6) |
| `docs/decisions/` | numbered decision records |
| `product.json` | single source of truth for naming |

## Dev setup

Toolchain (pinned in repo): Node 22, pnpm 10.18.3 (corepack), Rust 1.92.0,
uv (any recent), Python 3.12 via uv.

```sh
pnpm install            # JS workspace
pnpm build              # build all TS packages
cargo build             # Rust workspace
cd python/pylair && uv sync   # Python env (fetches managed CPython)
```

Docs: [security](docs/security.md), [protocol](docs/protocol-v1.md),
[cancellation](docs/cancellation.md), [codegen](docs/codegen.md),
[decisions](docs/decisions/), [ideas](docs/ideas.md).

## License

Dual-licensed under MIT or Apache-2.0 — pick whichever fits your project.
See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

Installers produced by `pylair build` are **unsigned** unless you configure
code signing — expect Gatekeeper (macOS) and SmartScreen (Windows)
warnings on unsigned builds. A first build downloads the bundled Python
runtime and Tauri's installer toolchain (WiX/NSIS on Windows) from
GitHub, so it needs network access once; later builds reuse the caches.
See [docs/build.md](docs/build.md).

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with or
endorsed by the Tauri project. Apps it produces bundle third-party
software (a standalone CPython build and your Python dependencies);
their licenses ship inside each bundle under `THIRD-PARTY-NOTICES`.
