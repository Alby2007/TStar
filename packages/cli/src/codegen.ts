/**
 * Type codegen: manifest (python -m tauri_plus.codegen) -> api.generated.ts.
 * Spec: docs/codegen.md.
 */

export interface Schema {
  type: string;
  items?: Schema | Schema[];
  opts?: Schema[];
  keys?: Schema;
  /** dict: value schema; literal/enum: raw values */
  values?: Schema | unknown[];
  name?: string;
  fields?: { name: string; required: boolean; schema: Schema }[];
}

export interface Param {
  name: string;
  kind: string;
  required: boolean;
  schema: Schema;
}

export interface Fn {
  name: string;
  kind: "sync" | "async" | "generator" | "isolated";
  doc: string | null;
  params: Param[];
  returns: Schema;
}

export interface Manifest {
  version: number;
  functions?: Fn[];
  error?: string;
}

const RESERVED = new Set(
  (
    "arguments await break case catch class const continue debugger default " +
    "delete do else enum eval export extends false finally for function if " +
    "implements import in instanceof interface let new null package private " +
    "protected public return static super switch this throw true try typeof " +
    "var void while with yield async of static get set"
  ).split(" "),
);

function ident(name: string): string {
  const cleaned = name.replace(/[^A-Za-z0-9_$]/g, "_");
  const base = /^[A-Za-z_$]/.test(cleaned) ? cleaned : `_${cleaned}`;
  return RESERVED.has(base) ? `${base}_` : base;
}

function camel(name: string): string {
  const out = ident(name.replace(/_+([a-zA-Z0-9])/g, (_, c) => c.toUpperCase()));
  return RESERVED.has(out) ? `${out}_` : out;
}

/** schema -> TS type expression */
function ts(s: Schema | undefined): string {
  if (!s) return "unknown";
  switch (s.type) {
    case "int":
    case "float":
      return "number";
    case "str":
      return "string";
    case "bool":
      return "boolean";
    case "none":
      return "null";
    case "bytes":
      return "Uint8Array";
    case "datetime":
      return "string"; // ISO 8601
    case "list":
      return `${ts(s.items as Schema)}[]`;
    case "tuple":
      return `[${((s.items as Schema[]) ?? []).map(ts).join(", ")}]`;
    case "dict":
      return !s.keys || s.keys.type === "str" || s.keys.type === "any"
        ? `Record<string, ${ts(s.values as Schema)}>`
        : "unknown"; // non-string keys can't round-trip through JSON objects
    case "union":
      return (s.opts ?? []).map(ts).join(" | ") || "unknown";
    case "literal":
    case "enum":
      return (s.values as unknown[] ?? []).map((v) => JSON.stringify(v)).join(" | ") ||
        "unknown";
    case "dataclass":
      return ident(s.name ?? "Anonymous");
    default:
      return "unknown";
  }
}

/** Recursively collect every dataclass schema reachable from a schema. */
function collectDataclasses(s: Schema | undefined, out: Map<string, Schema>): void {
  if (!s) return;
  if (s.type === "dataclass" && s.name) {
    if (!out.has(s.name)) {
      out.set(s.name, s);
      for (const f of s.fields ?? []) collectDataclasses(f.schema, out);
    }
    return;
  }
  // items: Schema | Schema[]; keys/values: Schema (dict) or primitives
  // (literal/enum) — the object guard sorts them out.
  for (const sub of [s.items, s.keys, s.values].flat()) {
    if (sub && typeof sub === "object") collectDataclasses(sub as Schema, out);
  }
  for (const o of s.opts ?? []) collectDataclasses(o, out);
}

const HELPERS = `function _b64encode(u8: Uint8Array): string {
  let s = "";
  for (let i = 0; i < u8.length; i += 0x8000)
    s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
  return btoa(s);
}
function _b64decode(b64: string): Uint8Array {
  const s = atob(b64);
  const u8 = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) u8[i] = s.charCodeAt(i);
  return u8;
}
// Uint8Array <-> {"$b64": "..."} anywhere in the payload (protocol wire JSON)
function _toWire(v: Record<string, unknown>): Record<string, unknown>;
function _toWire(v: unknown): unknown;
function _toWire(v: unknown): unknown {
  if (v instanceof Uint8Array) return { $b64: _b64encode(v) };
  if (Array.isArray(v)) return v.map(_toWire);
  if (v !== null && typeof v === "object")
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, x]) => [k, _toWire(x)]),
    );
  return v;
}
function _fromWire(v: unknown): unknown {
  if (v !== null && typeof v === "object") {
    const ks = Object.keys(v);
    if (ks.length === 1 && ks[0] === "$b64")
      return _b64decode((v as { $b64: string }).$b64);
    if (Array.isArray(v)) return v.map(_fromWire);
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, x]) => [
        k,
        _fromWire(x),
      ]),
    );
  }
  return v;
}
async function* _mapStream<T>(g: AsyncGenerator<unknown, void, void>) {
  for await (const x of g) yield _fromWire(x) as T;
}
`;

/** Render the manifest as a TypeScript module. */
export function emit(m: Manifest, source: string): string {
  if (m.error) throw new Error(`manifest error: ${m.error}`);
  const fns = m.functions ?? [];

  const dataclasses = new Map<string, Schema>();
  for (const f of fns) {
    for (const p of f.params) collectDataclasses(p.schema, dataclasses);
    collectDataclasses(f.returns, dataclasses);
  }

  const out: string[] = [
    `// GENERATED by tauri-plus codegen — do not edit.`,
    `// Source module: ${source}`,
    `import { call, stream, type CallOptions } from "@tauri-plus/client";`,
    "",
    HELPERS,
  ];

  for (const dc of dataclasses.values()) {
    const lines = (dc.fields ?? []).map(
      (f) => `  ${ident(f.name)}${f.required ? "" : "?"}: ${ts(f.schema)};`,
    );
    out.push(`export interface ${ident(dc.name!)} {\n${lines.join("\n")}\n}\n`);
  }

  for (const f of fns) {
    if (f.doc) out.push(`/** ${f.doc.split("\n")[0]} */`);
    const params = f.params.map(
      (p) => `${ident(p.name)}${p.required ? "" : "?"}: ${ts(p.schema)}`,
    );
    params.push("opts?: CallOptions");
    const inputObj = `{ ${f.params.map((p) => `${JSON.stringify(p.name)}: ${ident(p.name)}`).join(", ")} }`;
    const fname = camel(f.name);
    if (f.kind === "generator") {
      const item = ts(f.returns);
      out.push(
        `export function ${fname}(${params.join(", ")}): AsyncGenerator<${item}, void, void> {`,
        `  return _mapStream<${item}>(stream(${JSON.stringify(f.name)}, _toWire(${inputObj}), opts));`,
        `}`,
        "",
      );
    } else {
      const ret = ts(f.returns);
      out.push(
        `export function ${fname}(${params.join(", ")}): Promise<${ret}> {`,
        `  return call(${JSON.stringify(f.name)}, _toWire(${inputObj}), opts).then(_fromWire) as Promise<${ret}>;`,
        `}`,
        "",
      );
    }
  }
  return out.join("\n");
}
