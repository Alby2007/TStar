import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const bin = fileURLToPath(new URL("../dist/index.js", import.meta.url));

function scaffold(args, cwd) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8", cwd });
}

test("scaffolds a complete app skeleton", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold(["my-app"], dir);
  assert.equal(res.status, 0, res.stderr);
  const app = join(dir, "my-app");
  for (const f of [
    "pyproject.toml",
    "requirements.txt",
    "package.json",
    "backend/app.py",
    "src-tauri/Cargo.toml",
    "src-tauri/tauri.conf.json",
    "src-tauri/src/lib.rs",
    "src-tauri/capabilities/default.json",
    "src-tauri/icons/icon.icns",
    "src/App.tsx",
    "index.html",
  ]) {
    assert.ok(existsSync(join(app, f)), `missing ${f}`);
  }
  const conf = JSON.parse(readFileSync(join(app, "src-tauri/tauri.conf.json"), "utf8"));
  assert.equal(conf.productName, "my-app");
  assert.equal(conf.identifier, "com.example.myapp");
  const py = readFileSync(join(app, "pyproject.toml"), "utf8");
  assert.match(py, /app-module = "backend\.app"/);
  assert.match(py, /pylair>=0\.1/);
  rmSync(dir, { recursive: true });
});

test("--local wires all three deps at the checkout", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  const res = scaffold(["local-app", "--local", "/fake/repo"], dir);
  assert.equal(res.status, 0, res.stderr);
  const app = join(dir, "local-app");
  assert.match(readFileSync(join(app, "pyproject.toml"), "utf8"), /file:\/\/\/fake\/repo\/python\/pylair/);
  assert.match(readFileSync(join(app, "package.json"), "utf8"), /file:\/fake\/repo\/packages\/client/);
  assert.match(
    readFileSync(join(app, "src-tauri/Cargo.toml"), "utf8"),
    /tauri-plugin-pylair = \{ path = "\/fake\/repo\/crates\/tauri-plugin-pylair" \}/,
  );
  rmSync(dir, { recursive: true });
});

test("rejects bad names and non-empty dirs", () => {
  const dir = mkdtempSync(join(tmpdir(), "create-pylair-"));
  assert.notEqual(scaffold(["Bad Name"], dir).status, 0);
  assert.equal(scaffold(["ok-app"], dir).status, 0);
  assert.notEqual(scaffold(["ok-app"], dir).status, 0); // now non-empty
  rmSync(dir, { recursive: true });
});
