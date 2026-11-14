import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";

import { codegenEnv } from "../dist/dev.js";
import { importRoot } from "../dist/config.js";

describe("codegenEnv", () => {
  test("PYTHONPATH is the configured source's import root", () => {
    // source = "server" → the module `server.app` resolves from appDir,
    // the PARENT of the source dir — not <appDir>/server itself.
    const env = codegenEnv("/app", "server");
    assert.equal(env.PYTHONPATH, importRoot("/app", "server"));
    assert.equal(env.PYTHONPATH, join("/app"));
    assert.notEqual(env.PYTHONPATH, join("/app", "server"));
  });

  test("nested source dirs resolve their parent", () => {
    const env = codegenEnv("/app", "src/backend");
    assert.equal(env.PYTHONPATH, join("/app", "src"));
  });
});
