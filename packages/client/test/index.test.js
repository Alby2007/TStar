import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import {
  python,
  call,
  stream,
  cancel,
  setBridge,
  PythonError,
  _resetForTests,
} from "../dist/index.js";

/** Fake IPC bridge: records invokes, lets tests push events. */
function fakeBridge() {
  const calls = [];
  const listeners = new Map();
  let nextTask = 100;
  return {
    calls,
    emit(event, payload) {
      for (const cb of listeners.get(event) ?? []) cb(payload);
    },
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === "plugin:tauri-plus|python_call_stream") return nextTask++;
      if (cmd === "plugin:tauri-plus|python_cancel") return true;
      if (cmd === "plugin:tauri-plus|python_state") return "ready";
      return null;
    },
    async listen(event, cb) {
      if (!listeners.has(event)) listeners.set(event, []);
      listeners.get(event).push(cb);
      return () => listeners.get(event).splice(listeners.get(event).indexOf(cb), 1);
    },
  };
}

const streamDone = (task, result) => ({ task, done: true, result });
const streamErr = (task, code, message) => ({ task, done: true, error: { code, message } });
const streamChunk = (task, seq, value) => ({ task, seq, value, done: false });

beforeEach(() => _resetForTests());

test("call resolves with the terminal result", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("add", { a: 1, b: 2 });
  await new Promise((r) => setTimeout(r, 0)); // let invoke resolve, task=100
  b.emit("tauri-plus://stream", streamDone(100, 3));
  assert.equal(await p, 3);
});

test("call rejects with PythonError on runtime error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("boom");
  await new Promise((r) => setTimeout(r, 0));
  b.emit("tauri-plus://stream", streamErr(100, -32000, "kaboom"));
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32000);
});

test("abort signal sends python_cancel for the task", async () => {
  const b = fakeBridge();
  setBridge(b);
  const ac = new AbortController();
  const p = call("slow", {}, { signal: ac.signal });
  await new Promise((r) => setTimeout(r, 0));
  ac.abort();
  await new Promise((r) => setTimeout(r, 0));
  assert.ok(b.calls.some((c) => c.cmd === "plugin:tauri-plus|python_cancel" && c.args.task === 100));
  // runtime then reports TASK_CANCELLED as the terminal
  b.emit("tauri-plus://stream", streamErr(100, -32010, "task cancelled"));
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32010);
});

test("stream yields chunks then ends on terminal", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 3 });
  const collected = [];
  const consume = (async () => {
    for await (const v of it) collected.push(v);
  })();
  await new Promise((r) => setTimeout(r, 0));
  b.emit("tauri-plus://stream", streamChunk(100, 0, "a"));
  b.emit("tauri-plus://stream", streamChunk(100, 1, "b"));
  b.emit("tauri-plus://stream", streamChunk(100, 2, "c"));
  b.emit("tauri-plus://stream", streamDone(100, { done: true }));
  await consume;
  assert.deepEqual(collected, ["a", "b", "c"]);
});

test("stream throws PythonError on terminal error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter");
  const consume = (async () => {
    for await (const _ of it) { /* consume */ }
  })();
  await new Promise((r) => setTimeout(r, 0));
  b.emit("tauri-plus://stream", streamErr(100, -32031, "runtime crashed"));
  await assert.rejects(consume, (e) => e instanceof PythonError && e.code === -32031);
});

test("stream tops up pull credit as consumer drains", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("infinite");
  const consume = (async () => {
    let i = 0;
    for await (const _ of it) if (++i >= 40) break;
  })();
  await new Promise((r) => setTimeout(r, 0));
  for (let s = 0; s < 40; s++) b.emit("tauri-plus://stream", streamChunk(100, s, s));
  await consume;
  const grants = b.calls.filter((c) => c.cmd === "plugin:tauri-plus|python_grant_credit");
  assert.ok(grants.length >= 1 && grants.every((g) => g.args.task === 100));
});

test("cancel() forwards task id", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await cancel(77), true);
  assert.deepEqual(b.calls.at(-1), { cmd: "plugin:tauri-plus|python_cancel", args: { task: 77 } });
});

test("state() and onState() expose lifecycle", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await python.state(), "ready");
  const seen = [];
  const off = await python.onState((s) => seen.push(s));
  b.emit("tauri-plus://state", { from: "ready", to: "restarting", detail: "x" });
  assert.equal(seen[0].to, "restarting");
  off();
});

test("python namespace exposes the API surface", () => {
  for (const k of ["call", "stream", "cancel", "state", "shutdown", "onState", "onStderr", "onExposes"]) {
    assert.equal(typeof python[k], "function", `python.${k}`);
  }
});
