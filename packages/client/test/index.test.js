import { test } from "node:test";
import assert from "node:assert/strict";
import { python } from "../dist/index.js";

test("python client stub exports an object", () => {
  assert.equal(typeof python, "object");
});
