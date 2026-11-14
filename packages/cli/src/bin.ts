#!/usr/bin/env node
// Dedicated bin entry so cli.js stays importable in tests — an argv[1]
// guard in cli.js would miss bin shims (node_modules/.bin symlinks,
// npx, pnpm command shims) where argv[1] is the shim, not this file.
import { main } from "./cli.js";

Promise.resolve(main(process.argv.slice(2))).then(process.exit);
