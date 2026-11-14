import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import { Readable } from "node:stream";
import { createHash } from "node:crypto";

import { matchGlob } from "../dist/glob.js";
import { stageNotices, download, ensureTarball } from "../dist/build.js";
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

describe("download", () => {
  // A fake ClientRequest: what the injected `get` must return.
  const fakeRequest = () => {
    const req = new EventEmitter();
    req.setTimeout = (ms, fn) => {
      req._t = setTimeout(fn, ms);
      return req;
    };
    req.destroy = (e) => {
      clearTimeout(req._t);
      req.emit("error", e ?? new Error("destroyed"));
    };
    return req;
  };
  const fakeGet = (handler) => (u, cb) => {
    const req = fakeRequest();
    queueMicrotask(() => handler(u, cb, req));
    return req;
  };
  const response = (body, { status = 200, headers = {} } = {}) => {
    const r = Readable.from(body);
    r.statusCode = status;
    r.headers = headers;
    return r;
  };

  test("a mid-body drop leaves nothing at the final path", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    const dropped = Readable.from(
      (async function* () {
        yield Buffer.from("first-half");
        throw new Error("connection reset");
      })(),
    );
    dropped.statusCode = 200;
    dropped.headers = { "content-length": "32" };
    await assert.rejects(
      download("https://x/asset", to, { get: fakeGet((u, cb) => cb(dropped)) }),
      /reset|dropped|incomplete/i,
    );
    assert.ok(!existsSync(to), "partial body must not sit at the final path");
    assert.ok(!existsSync(`${to}.part`), ".part must be cleaned up");

    // The next run succeeds — a partial must never poison the cache.
    await download(
      "https://x/asset",
      to,
      { get: fakeGet((u, cb) => cb(response([Buffer.from("full-body")]))) },
    );
    assert.equal(readFileSync(to, "utf-8"), "full-body");
    rmSync(dir, { recursive: true });
  });

  test("a redirect loop hits the cap", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    let n = 0;
    const get = fakeGet((u, cb) =>
      cb(response([], { status: 302, headers: { location: `${u}/${++n}` } })),
    );
    await assert.rejects(download("https://x/a", to, { get }), /redirect/i);
    rmSync(dir, { recursive: true });
  });

  test("a stalled server hits the idle timeout", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-dl-"));
    const to = join(dir, "asset.tar.gz");
    const stalled = new Readable({ read() {} }); // headers, then nothing
    stalled.statusCode = 200;
    stalled.headers = {};
    await assert.rejects(
      download("https://x/s", to, { get: fakeGet((u, cb) => cb(stalled)), timeoutMs: 50 }),
      /timed out|timeout/i,
    );
    assert.ok(!existsSync(to));
    assert.ok(!existsSync(`${to}.part`));
    rmSync(dir, { recursive: true });
  });
});

describe("ensureTarball", () => {
  const sha = (s) => createHash("sha256").update(s).digest("hex");

  test("a corrupt cached file is deleted and re-fetched once", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cache-"));
    const to = join(dir, "asset.tar.gz");
    writeFileSync(to, "corrupt-bytes");
    const want = sha(Buffer.from("good-bytes"));
    let calls = 0;
    const dl = async (u, dest) => {
      calls++;
      writeFileSync(dest, "good-bytes");
    };
    await ensureTarball("https://x/asset", to, want, dl);
    assert.equal(calls, 1, "re-download exactly once");
    assert.equal(readFileSync(to, "utf-8"), "good-bytes");
    rmSync(dir, { recursive: true });
  });

  test("a second mismatch refuses and names the cache file", async () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-cache-"));
    const to = join(dir, "asset.tar.gz");
    writeFileSync(to, "corrupt-bytes");
    const want = sha(Buffer.from("good-bytes"));
    const bad = async (u, dest) => writeFileSync(dest, "also-bad");
    const err = await ensureTarball("https://x/asset", to, want, bad).then(
      () => null,
      (e) => e,
    );
    assert.ok(err, "expected a refusal");
    assert.match(err.message, /SHA256|mismatch|refusing/i);
    assert.ok(err.message.includes("asset.tar.gz"), "message must name the cache file");
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

describe("stageNotices", () => {
  test("collects pylair, interpreter and wheel licenses", () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-notices-"));
    const res = join(dir, "resources");
    // interpreter tree: licenses/ dir + a top-level LICENSE.txt
    mkdirSync(join(res, "python", "licenses"), { recursive: true });
    writeFileSync(join(res, "python", "licenses", "openssl.txt"), "Apache-2.0 openssl text");
    writeFileSync(join(res, "python", "LICENSE.txt"), "PSF-2.0 text");
    // wheels: one with a licenses dir, one with METADATA only
    mkdirSync(join(res, "site-packages", "httpx-0.28.1.dist-info", "licenses"), {
      recursive: true,
    });
    writeFileSync(
      join(res, "site-packages", "httpx-0.28.1.dist-info", "licenses", "LICENSE.md"),
      "BSD-3 httpx text",
    );
    mkdirSync(join(res, "site-packages", "foo-1.0.dist-info"), { recursive: true });
    writeFileSync(
      join(res, "site-packages", "foo-1.0.dist-info", "METADATA"),
      "Metadata-Version: 2.4\nName: foo\nLicense-Expression: MIT\n",
    );
    // pylair's own license files
    const pd = join(dir, "pylair-pkg");
    mkdirSync(pd, { recursive: true });
    writeFileSync(join(pd, "LICENSE-MIT"), "mit");
    writeFileSync(join(pd, "LICENSE-APACHE"), "apache");

    stageNotices(res, pd);

    const out = join(res, "THIRD-PARTY-NOTICES");
    assert.ok(existsSync(join(out, "pylair", "LICENSE-MIT")));
    assert.ok(existsSync(join(out, "python", "licenses", "openssl.txt")));
    assert.ok(existsSync(join(out, "python", "LICENSE.txt")));
    assert.ok(existsSync(join(out, "httpx", "licenses", "LICENSE.md")));
    assert.match(
      String(readFileSync(join(out, "foo", "METADATA-LICENSE.txt"))),
      /License-Expression: MIT/,
    );
    const idx = String(readFileSync(join(out, "INDEX.txt")));
    for (const name of ["pylair", "python", "httpx", "foo"])
      assert.match(idx, new RegExp(`${name}/`));
    rmSync(dir, { recursive: true });
  });

  test("writes a placeholder NOTICE when the interpreter has no licenses", () => {
    const dir = mkdtempSync(join(tmpdir(), "pylair-notices-"));
    const res = join(dir, "resources");
    mkdirSync(join(res, "python", "bin"), { recursive: true });
    mkdirSync(join(dir, "pd"), { recursive: true });
    stageNotices(res, join(dir, "pd"));
    assert.ok(existsSync(join(res, "THIRD-PARTY-NOTICES", "python", "NOTICE.txt")));
    rmSync(dir, { recursive: true });
  });
});

describe("stageSource", () => {
  function srcTree() {
    const dir = mkdtempSync(join(tmpdir(), "pylair-stage-"));
    for (const f of [
      "app.py",
      ".env",
      ".env.local",
      "key.pem",
      ".hidden/x.txt",
      "sub/id_rsa",
      "sub/ok.py",
    ]) {
      const p = join(dir, f);
      mkdirSync(dirname(p), { recursive: true });
      writeFileSync(p, "x");
    }
    return dir;
  }

  test("stages normal files, skips and names secret-looking paths", async () => {
    const { stageSource } = await import("../dist/build.js");
    const src = srcTree();
    const dest = mkdtempSync(join(tmpdir(), "pylair-staged-"));
    const lines = [];
    const skipped = stageSource(src, dest, [], (m) => lines.push(m));

    for (const ok of ["app.py", join("sub", "ok.py")])
      assert.ok(existsSync(join(dest, ok)), `${ok} should be staged`);
    for (const denied of [".env", ".env.local", "key.pem", join(".hidden", "x.txt"), join("sub", "id_rsa")])
      assert.ok(!existsSync(join(dest, denied)), `${denied} must not ship`);
    for (const named of [".env", ".env.local", "key.pem", ".hidden", "sub/id_rsa"])
      assert.ok(skipped.includes(named), `skipped list missing ${named}`);
    // The warning counts, names, and tells the author how to opt in.
    assert.match(lines[0], /skipped 5/);
    for (const named of [".env", "key.pem", ".hidden", "sub/id_rsa"])
      assert.ok(lines[0].includes(named), `warning names ${named}`);
    assert.match(lines.join("\n"), /\[tool\.pylair\] include/);
    rmSync(src, { recursive: true });
    rmSync(dest, { recursive: true });
  });

  test("include reaches inside a denied directory", async () => {
    const { stageSource } = await import("../dist/build.js");
    const src = srcTree();
    const dest = mkdtempSync(join(tmpdir(), "pylair-staged-"));
    const lines = [];
    const skipped = stageSource(src, dest, [".hidden/x.txt"], (m) => lines.push(m));

    assert.ok(existsSync(join(dest, ".hidden", "x.txt")), "included file must stage");
    assert.ok(!existsSync(join(dest, ".env")), ".env still skipped");
    assert.ok(!skipped.includes(".hidden"), ".hidden not denied — it holds an include");
    assert.ok(!lines.join("\n").includes("x.txt"), "warning must not name the included file");
    rmSync(src, { recursive: true });
    rmSync(dest, { recursive: true });
  });
});

describe("meetsMinPython", () => {
  const cases = [
    ["3.9", false],
    ["3.10", false],
    ["3.11", false],
    ["3.12", true],
    ["3.13", true],
    ["4.0", true],
    ["garbage", false],
    ["", false],
  ];
  for (const [v, want] of cases) {
    test(`meetsMinPython(${JSON.stringify(v)}) → ${want}`, async () => {
      const { meetsMinPython } = await import("../dist/doctor.js");
      assert.equal(meetsMinPython(v, "3.12"), want);
    });
  }
});
