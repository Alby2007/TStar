import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { emit } from "../dist/codegen.js";
import { loadManifest } from "../dist/manifest.js";

const FN = (over) => ({
  name: "f",
  kind: "sync",
  doc: null,
  params: [],
  returns: { type: "none" },
  ...over,
});
const M = (functions) => ({ version: 1, functions });

test("emits plain call wrappers", () => {
  const ts = emit(
    M([
      FN({
        name: "add_them",
        params: [
          { name: "a", kind: "P", required: true, schema: { type: "int" } },
          { name: "b", kind: "P", required: true, schema: { type: "float" } },
        ],
        returns: { type: "float" },
      }),
    ]),
    "myapp",
  );
  assert.match(ts, /export function addThem\(a: number, b: number, opts\?: CallOptions\): Promise<number>/);
  assert.match(ts, /call\("add_them", _toWire\(\{ "a": a, "b": b \}\), opts\)/);
});

test("optional params become ?", () => {
  const ts = emit(
    M([
      FN({
        params: [
          { name: "a", kind: "P", required: true, schema: { type: "int" } },
          { name: "b", kind: "P", required: false, schema: { type: "str" } },
        ],
      }),
    ]),
    "m",
  );
  assert.match(ts, /a: number, b\?: string/);
});

test("generators emit stream + async generator", () => {
  const ts = emit(
    M([FN({ name: "ticks", kind: "generator", returns: { type: "int" } })]),
    "m",
  );
  assert.match(ts, /AsyncGenerator<number, void, void>/);
  assert.match(ts, /_mapStream<number>\(stream\("ticks"/);
});

test("dataclasses dedupe into interfaces", () => {
  const point = {
    type: "dataclass",
    name: "Point",
    fields: [
      { name: "x", required: true, schema: { type: "float" } },
      { name: "y", required: true, schema: { type: "float" } },
      { name: "label", required: false, schema: { type: "str" } },
    ],
  };
  const ts = emit(
    M([
      FN({ name: "a", params: [{ name: "p", kind: "P", required: true, schema: point }], returns: point }),
      FN({ name: "b", returns: point }),
    ]),
    "m",
  );
  assert.equal(ts.match(/export interface Point/g).length, 1);
  assert.match(ts, /x: number;\n  y: number;\n  label\?: string;/);
});

test("nested dataclass fields are emitted", () => {
  const inner = {
    type: "dataclass",
    name: "Inner",
    fields: [{ name: "v", required: true, schema: { type: "int" } }],
  };
  const outer = {
    type: "dataclass",
    name: "Outer",
    fields: [{ name: "inner", required: true, schema: inner }],
  };
  const ts = emit(M([FN({ returns: outer })]), "m");
  assert.match(ts, /export interface Outer \{\n  inner: Inner;\n\}/);
  assert.match(ts, /export interface Inner \{\n  v: number;\n\}/);
});

test("type mapping", () => {
  const cases = [
    [{ type: "list", items: { type: "int" } }, "number\\[\\]"],
    [{ type: "tuple", items: [{ type: "int" }, { type: "str" }] }, "\\[number, string\\]"],
    [{ type: "dict", keys: { type: "str" }, values: { type: "int" } }, "Record<string, number>"],
    [{ type: "dict", keys: { type: "int" }, values: { type: "int" } }, "unknown"],
    [{ type: "union", opts: [{ type: "str" }, { type: "none" }] }, "string \\| null"],
    [{ type: "literal", values: ["a", "b"] }, '"a" \\| "b"'],
    [{ type: "enum", values: ["red", "blue"] }, '"red" \\| "blue"'],
    [{ type: "bytes" }, "Uint8Array"],
    [{ type: "datetime" }, "string"],
    [{ type: "any" }, "unknown"],
  ];
  for (const [schema, want] of cases) {
    const ts = emit(
      M([FN({ params: [{ name: "x", kind: "P", required: true, schema }] })]),
      "m",
    );
    assert.match(ts, new RegExp(`x: ${want}`), JSON.stringify(schema));
  }
});

test("reserved words get a suffix but keep wire name", () => {
  const ts = emit(
    M([
      FN({
        name: "f",
        params: [{ name: "class", kind: "P", required: true, schema: { type: "str" } }],
      }),
    ]),
    "m",
  );
  assert.match(ts, /class_: string/);
  assert.match(ts, /"class": class_/);
});

test("docs become jsdoc, manifest errors throw", () => {
  const ts = emit(M([FN({ doc: "hello\nworld" })]), "m");
  assert.match(ts, /\/\*\* hello \*\//);
  assert.throws(() => emit({ version: 1, error: "boom" }, "m"), /boom/);
});

test("snake_case names stay the wire name", () => {
  const ts = emit(M([FN({ name: "make_thing" })]), "m");
  assert.match(ts, /export function makeThing\(/);
  assert.match(ts, /call\("make_thing"/);
});

// ---- manifest loading (real python subprocess) ----------------------------

const hasPython = (() => {
  try {
    execFileSync(
      "python3",
      ["-c", "import sys;sys.exit(0 if sys.version_info>=(3,12) else 1)"],
      { stdio: "pipe" },
    );
    return true;
  } catch {
    return false;
  }
})();

test("loadManifest reads a manifest from a real module", { skip: !hasPython }, () => {
  const dir = mkdtempSync(path.join(tmpdir(), "tp-codegen-"));
  writeFileSync(
    path.join(dir, "appmod.py"),
    "from tauri_plus import expose\n@expose\ndef hi(name: str) -> str:\n    return name\n",
  );
  const src = path.resolve("../../python/tauri_plus/src");
  const m = loadManifest({
    python: "python3",
    module: "appmod",
    cwd: dir,
    env: { PYTHONPATH: src },
  });
  const fn = m.functions.find((f) => f.name === "hi");
  assert.equal(fn.params[0].schema.type, "str");
});

test("loadManifest surfaces module errors", { skip: !hasPython }, () => {
  assert.throws(
    () => loadManifest({ python: "python3", module: "no_such_xyz" }),
    /codegen manifest failed/,
  );
});
