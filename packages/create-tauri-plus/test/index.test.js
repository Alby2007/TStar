import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const bin = fileURLToPath(new URL("../dist/index.js", import.meta.url));

test("stub exits 1 and points at M4", () => {
  const res = spawnSync(process.execPath, [bin], { encoding: "utf8" });
  assert.equal(res.status, 1);
  assert.match(res.stdout, /not implemented yet/);
});
