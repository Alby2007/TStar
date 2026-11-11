# @pylair/cli

The `pylair` command-line tool: run a supervised Python backend behind
your Tauri frontend in dev, then bundle Python and your dependencies
into installable desktop apps — no Python required on target machines.

```sh
pylair dev       # dev mode: hot reload + supervised Python runtime
pylair build     # signed/unsigned installers for Windows, macOS, Linux
pylair codegen   # TypeScript types from your @expose'd Python functions
pylair doctor    # diagnose environment and packaging issues
```

The installed command is `pylair` even though the package is scoped.

**Release candidate:** install with an exact pin or the `next` tag —
`npm i -D @pylair/cli@next` / `npm i -D @pylair/cli@0.1.0-rc.1`.
Installers produced by `pylair build` are **unsigned** unless you
configure code signing — expect Gatekeeper/SmartScreen warnings.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with
or endorsed by the Tauri project.

Licensed under MIT OR Apache-2.0 — see LICENSE-MIT and LICENSE-APACHE.
