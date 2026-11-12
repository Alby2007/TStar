# @pylair/client

TypeScript client SDK for Pylair apps: typed calls to exposed Python
functions, streaming, cancellation, lifecycle events, and wire/base64
helpers — everything the webview side of a Pylair app needs.

```ts
import { call, stream } from "@pylair/client";

const sum = await call<number>("add", { a: 1, b: 2 });

for await (const token of stream<string>("generate", { prompt })) {
  // tokens arrive as Python yields them
}
```

**Release candidate:** install with an exact pin or the `next` tag —
`npm i @pylair/client@next` / `npm i @pylair/client@0.1.0-rc.3`.

Pylair builds on [Tauri](https://tauri.app) but is not affiliated with
or endorsed by the Tauri project.

Licensed under MIT OR Apache-2.0 — see LICENSE-MIT and LICENSE-APACHE.
