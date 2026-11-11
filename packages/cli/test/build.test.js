import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

import { matchGlob } from "../dist/glob.js";
import { DEFAULT_PRUNE, prune } from "../dist/prune.js";
import { loadConfig } from "../dist/config.js";

describe("matchGlob", () => {
  test("literal and single-star", () => {
    assert.ok(matchGlob("python/include", "python/include"));
    assert.ok(matchGlob("python/include", "python/include/pyconfig.h")); // dir prefix
    assert.ok(matchGlob("*.a", "libfoo.a"));
    assert.ok(!matchGlob("*.a", "x/libfoo.a"));
    assert.ok(matchGlob("python/lib/*.a", "python/lib/libssl.a"));
    assert.ok(!matchGlob("python/lib/*.a", "python/lib/x/libssl.a"));
  });

  test("double-star crosses dirs", () => {
    assert.ok(matchGlob("python/lib/**/*.a", "python/lib/x/y/z.a"));
    assert.ok(matchGlob("site-packages/**/__pycache__", "site-packages/a/b/__pycache__"));
    assert.ok(matchGlob("python/lib/python*/test", "python/lib/python3.12/test"));
    assert.ok(matchGlob("python/lib/python*/test", "python/lib/python3.12/test/x.py"));
    assert.ok(!matchGlob("python/lib/python*/test", "python/lib/python3.12/testing"));
  });

  test("question mark is single char", () => {
    assert.ok(matchGlob("a?c", "abc"));
    assert.ok(!matchGlob("a?c", "a/c"));
  });
});

describe("prune", () => {
  function tree(files) {
    const dir = mkdtempSync(join(tmpdir(), "pylair-prune-"));
    for (const f of files) {
      const p = join(dir, f);
      mkdirSync(join(p, ".."), { recursive: true });
      writeFileSync(p, "x".repeat(16));
    }
    return dir;
  }

  test("removes denied dirs, keeps the rest", () => {
    const dir = tree([
      "python/lib/python3.12/test/x.py",
      "python/lib/python3.12/os.py",
      "python/include/Python.h",
      "site-packages/pip/__init__.py",
      "site-packages/numpy/__init__.py",
      "site-packages/numpy/tests/t.py",
      "backend/app.py",
      "backend/__pycache__/app.pyc",
    ]);
    const r = prune(dir, DEFAULT_PRUNE);
    assert.ok(r.removed >= 5);
    assert.ok(!existsSync(join(dir, "python/include")));
    assert.ok(!existsSync(join(dir, "python/lib/python3.12/test")));
    assert.ok(!existsSync(join(dir, "site-packages/pip")));
    assert.ok(!existsSync(join(dir, "site-packages/numpy/tests")));
    assert.ok(existsSync(join(dir, "python/lib/python3.12/os.py")));
    assert.ok(existsSync(join(dir, "site-packages/numpy/__init__.py")));
    assert.ok(existsSync(join(dir, "backend/app.py")));
    rmSync(dir, { recursive: true });
  });

  test("topmost dir wins; nested hits counted once", () => {
    const dir = tree(["site-packages/pip/_internal/x.py", "site-packages/pip/y.py"]);
    const r = prune(dir, ["site-packages/pip"]);
    assert.equal(r.removed, 1);
    rmSync(dir, { recursive: true });
  });
});

describe("loadConfig", () => {
  const py = (() => {
    for (const c of ["python3", "python"]) {
      const r = spawnSync(c, ["-c", "import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)"]);
      if (r.status === 0) return c;
    }
    return null;
  })();

  test("parses [tool.pylair]", { skip: !py }, () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cfg-"));
    writeFileSync(
      join(dir, "pyproject.toml"),
      `[project]\nname="app"\n[tool.pylair]\napp-module="app.backend"\npython-version="3.13"\nprune=["extra/dir"]\n`,
    );
    const cfg = loadConfig(dir, py);
    assert.equal(cfg.appModule, "app.backend");
    assert.equal(cfg.pythonVersion, "3.13");
    assert.equal(cfg.source, "backend");
    assert.deepEqual(cfg.prune, ["extra/dir"]);
    rmSync(dir, { recursive: true });
  });

  test("requires app-module", { skip: !py }, () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cfg-"));
    writeFileSync(join(dir, "pyproject.toml"), "[project]\nname='a'\n[tool.pylair]\n");
    assert.throws(() => loadConfig(dir, py), /app-module is required/);
    rmSync(dir, { recursive: true });
  });

  test("errors without pyproject", () => {
    assert.throws(() => loadConfig("/nonexistent", "python3"), /no pyproject.toml/);
  });
});
