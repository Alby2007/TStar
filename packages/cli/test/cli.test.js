import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const cli = fileURLToPath(new URL("../dist/cli.js", import.meta.url));

test("--version prints version", () => {
  const out = execFileSync(process.execPath, [cli, "--version"], { encoding: "utf8" });
  assert.equal(out.trim(), "0.0.0");
});

test("--help lists commands and exits 0", () => {
  const out = execFileSync(process.execPath, [cli, "--help"], { encoding: "utf8" });
  for (const cmd of ["dev", "build", "doctor", "codegen"]) {
    assert.ok(out.includes(cmd), `help output should list '${cmd}'`);
  }
});
