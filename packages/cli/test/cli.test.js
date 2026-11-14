import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { parseBuildArgs } from "../dist/cli.js";

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

describe("parseBuildArgs", () => {
  test("a pass-through --flag forwards its value", () => {
    const p = parseBuildArgs(["--config", "tauri.alt.conf.json"]);
    assert.deepEqual(p.tauriArgs, ["--config", "tauri.alt.conf.json"]);
  });

  test("--flag=value forwards unchanged", () => {
    const p = parseBuildArgs(["--config=tauri.alt.conf.json"]);
    assert.deepEqual(p.tauriArgs, ["--config=tauri.alt.conf.json"]);
  });

  test("everything after -- is forwarded verbatim", () => {
    const p = parseBuildArgs(["--verbose", "--", "--literal", "v", "-x"]);
    assert.deepEqual(p.tauriArgs, ["--verbose", "--literal", "v", "-x"]);
  });

  test("a flag followed by another flag takes no value", () => {
    const p = parseBuildArgs(["--verbose", "--debug"]);
    assert.deepEqual(p.tauriArgs, ["--verbose", "--debug"]);
  });

  test("own flags still parse and are not forwarded", () => {
    const p = parseBuildArgs([
      "--dir", "/some/app",
      "--config", "c.json",
      "--bundles", "msi,nsis",
      "--skip-fetch",
    ]);
    assert.equal(p.dir, "/some/app");
    assert.equal(p.bundles, "msi,nsis");
    assert.equal(p.skipFetch, true);
    assert.deepEqual(p.tauriArgs, ["--config", "c.json", "--bundles", "msi,nsis"]);
  });
});
