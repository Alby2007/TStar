# Tauri+ (working title)

Desktop framework for Python and local AI applications: run one command,
get a Tauri desktop app with a modern web front end and a managed,
bundled Python runtime — no Python install required for end users.

See [PROJECT_BRIEF.md](PROJECT_BRIEF.md) for goals, architecture and
milestones. The name is a placeholder; naming lives in `product.json`.

## Status

Early scaffold. Milestones and order: `M0` packaging spike → `M1`
supervisor/protocol → `M2` typed bridge → `M3` streaming/cancellation →
`M4` CLI/scaffolder → `M5` Ollama integration → `M6` reference app + RC.

## Layout

| Path | What |
|---|---|
| `crates/tauri-plugin-tauri-plus` | Rust plugin: supervisor, IPC, permissions |
| `packages/client` | `@tauri-plus/client` — typed calls, streams, cancellation |
| `packages/cli` | `tauri-plus` CLI — dev, build, doctor, codegen |
| `packages/create-tauri-plus` | project scaffolder |
| `python/tauri_plus` | Python runtime — `@expose`, protocol server, tasks |
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
cd python/tauri_plus && uv sync   # Python env (fetches managed CPython)
```

Docs: [security](docs/security.md), [protocol](docs/protocol-v1.md),
[cancellation](docs/cancellation.md), [codegen](docs/codegen.md),
[decisions](docs/decisions/), [ideas](docs/ideas.md).
