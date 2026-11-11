# tauri-plugin-pylair

Tauri plugin that embeds and supervises a Python runtime inside a desktop
app: lifecycle management, a typed IPC bridge, streaming, cancellation,
health checks and process-tree containment on Windows, macOS and Linux.

Part of [Pylair](https://github.com/PylairHQ/Pylair), a desktop framework
for Python and local-AI applications. Apps scaffolded with
`npx create-pylair` depend on this crate; direct use is possible but the
[`@pylair/client`](../packages/client) SDK and `pylair` CLI are the
supported path.

License: MIT OR Apache-2.0 — see LICENSE-MIT / LICENSE-APACHE.
