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
      if (cmd === "plugin:pylair|python_call_stream") return nextTask++;
      if (cmd === "plugin:pylair|python_cancel") return true;
      if (cmd === "plugin:pylair|python_state") return "ready";
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
  b.emit("pylair://stream", streamDone(100, 3));
  assert.equal(await p, 3);
});

test("call rejects with PythonError on runtime error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("boom");
  await new Promise((r) => setTimeout(r, 0));
  b.emit("pylair://stream", streamErr(100, -32000, "kaboom"));
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
  assert.ok(b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100));
  // runtime then reports TASK_CANCELLED as the terminal
  b.emit("pylair://stream", streamErr(100, -32010, "task cancelled"));
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
  b.emit("pylair://stream", streamChunk(100, 0, "a"));
  b.emit("pylair://stream", streamChunk(100, 1, "b"));
  b.emit("pylair://stream", streamChunk(100, 2, "c"));
  b.emit("pylair://stream", streamDone(100, { done: true }));
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
  b.emit("pylair://stream", streamErr(100, -32031, "runtime crashed"));
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
  for (let s = 0; s < 40; s++) b.emit("pylair://stream", streamChunk(100, s, s));
  await consume;
  const grants = b.calls.filter((c) => c.cmd === "plugin:pylair|python_grant_credit");
  assert.ok(grants.length >= 1 && grants.every((g) => g.args.task === 100));
});

test("cancel() forwards task id", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await cancel(77), true);
  assert.deepEqual(b.calls.at(-1), { cmd: "plugin:pylair|python_cancel", args: { task: 77 } });
});

test("state() and onState() expose lifecycle", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await python.state(), "ready");
  const seen = [];
  const off = await python.onState((s) => seen.push(s));
  b.emit("pylair://state", { from: "ready", to: "restarting", detail: "x" });
  assert.equal(seen[0].to, "restarting");
  off();
});

test("wire JSON: Uint8Array <-> $b64 inside call/stream", async () => {
  const b = fakeBridge();
  setBridge(b);
  // top-level + nested bytes encode on the way out
  const p = call("roundtrip", {
    data: new Uint8Array([0, 1, 2]),
    nested: { blob: new Uint8Array([255]) },
  });
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(b.calls[0].args.input.data, { $b64: "AAEC" });
  assert.deepEqual(b.calls[0].args.input.nested.blob, { $b64: "/w==" });
  // $b64 tags decode on the way back
  b.emit("pylair://stream", streamDone(100, { $b64: "AgEA" }));
  const result = await p;
  assert.ok(result instanceof Uint8Array);
  assert.deepEqual([...result], [2, 1, 0]);
});

test("wire JSON: stream chunks decode per item", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("gen");
  const collected = [];
  const consume = (async () => {
    for await (const v of it) collected.push(v);
  })();
  await new Promise((r) => setTimeout(r, 0));
  b.emit("pylair://stream", streamChunk(100, 0, { $b64: "aGk=" }));
  b.emit("pylair://stream", streamDone(100, { done: true }));
  await consume;
  assert.ok(collected[0] instanceof Uint8Array);
  assert.deepEqual([...collected[0]], [104, 105]); // "hi"
});

test("python namespace exposes the API surface", () => {
  for (const k of [
    "call",
    "stream",
    "cancel",
    "retry",
    "state",
    "shutdown",
    "onState",
    "onStderr",
    "onExposes",
  ]) {
    assert.equal(typeof python[k], "function", `python.${k}`);
  }
});

/** Bridge whose invoke emits stream events BEFORE it resolves — what the
 *  Rust side can do: the forwarder task emits as soon as Python replies,
 *  racing the invoke response that carries the task id. */
function earlyBridge(eventsFor) {
  const b = fakeBridge();
  const inner = b.invoke.bind(b);
  b.invoke = async (cmd, args) => {
    const task = await inner(cmd, args);
    if (cmd === "plugin:pylair|python_call_stream") {
      for (const ev of eventsFor(task)) b.emit("pylair://stream", ev);
    }
    return task;
  };
  return b;
}

test("call resolves when the terminal arrives before invoke resolves", async () => {
  const b = earlyBridge((t) => [streamDone(t, 42)]);
  setBridge(b);
  const r = await Promise.race([
    call("fast"),
    new Promise((_, rej) => setTimeout(() => rej(new Error("call stayed pending")), 500)),
  ]);
  assert.equal(r, 42);
});

test("call rejects when an early terminal is an error", async () => {
  const b = earlyBridge((t) => [streamErr(t, -32000, "early boom")]);
  setBridge(b);
  await assert.rejects(
    Promise.race([
      call("fast"),
      new Promise((_, rej) => setTimeout(() => rej(new Error("call stayed pending")), 500)),
    ]),
    (e) => e instanceof PythonError && e.code === -32000,
  );
});

test("stream keeps early chunks and the early terminal, in order", async () => {
  const b = earlyBridge((t) => [streamChunk(t, 0, "a"), streamChunk(t, 1, "b"), streamDone(t, null)]);
  setBridge(b);
  const got = [];
  const run = (async () => {
    for await (const v of stream("fast")) got.push(v);
  })();
  await Promise.race([
    run,
    new Promise((_, rej) => setTimeout(() => rej(new Error("stream stayed pending")), 500)),
  ]);
  assert.deepEqual(got, ["a", "b"]);
});

test("listener is installed before the first invoke returns", async () => {
  // A bridge whose listen() resolves late: events emitted by an invoke that
  // starts after listen() was *requested* must still be delivered.
  const b = fakeBridge();
  const realListen = b.listen.bind(b);
  b.listen = async (ev, cb) => {
    await new Promise((r) => setTimeout(r, 20));
    return realListen(ev, cb);
  };
  const inner = b.invoke.bind(b);
  b.invoke = async (cmd, args) => {
    const task = await inner(cmd, args);
    if (cmd === "plugin:pylair|python_call_stream") b.emit("pylair://stream", streamDone(task, "ok"));
    return task;
  };
  setBridge(b);
  const r = await Promise.race([
    call("fast"),
    new Promise((_, rej) => setTimeout(() => rej(new Error("call stayed pending")), 500)),
  ]);
  assert.equal(r, "ok");
});

test("breaking out of a stream cancels the producer", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 1000 });
  const first = it.next();
  await new Promise((r) => setTimeout(r, 0));
  b.emit("pylair://stream", streamChunk(100, 0, "a"));
  assert.equal((await first).value, "a");
  await it.return(undefined); // what `break` does
  assert.ok(
    b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100),
    "abandoning the iterator must send python_cancel",
  );
});

test("a stream that finished normally does not send a redundant cancel", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 1 });
  const first = it.next();
  await new Promise((r) => setTimeout(r, 0));
  b.emit("pylair://stream", streamChunk(100, 0, "a"));
  await first;
  const second = it.next();
  b.emit("pylair://stream", streamDone(100, null));
  assert.equal((await second).done, true);
  assert.ok(!b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});
