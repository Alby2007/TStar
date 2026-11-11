import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { importRoot } from "../dist/config.js";

test("import root is the parent of the source dir", () => {
  // "backend.app" lives at <base>/backend/app.py, so <base> must be on PYTHONPATH.
  assert.equal(importRoot("/res", "backend"), join("/res", "."));
  assert.equal(importRoot("/res", "src/backend"), join("/res", "src"));
});
