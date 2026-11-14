import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import {
  python,
  call,
  stream,
  cancel,
  setBridge,
  PythonError,
  LimitExceededError,
  _resetForTests,
} from "../dist/index.js";

/**
 * Fake IPC bridge: records invokes, lets tests push events. Per-call
 * delivery — `invokeChannel` captures the callback bound at invoke time
 * and `emitTask` delivers to that call only (what `Channel::send` does).
 */
function fakeBridge() {
  const calls = [];
  const listeners = new Map();
  const channels = new Map(); // task -> onEvent
  let nextTask = 100;
  return {
    calls,
    channels,
    emit(event, payload) {
      for (const cb of listeners.get(event) ?? []) cb(payload);
    },
    emitTask(task, ev) {
      const cb = channels.get(task);
      assert.ok(cb, `no channel registered for task ${task}`);
      cb(ev);
    },
    async invoke(cmd, args) {
      calls.push({ cmd, args });
      if (cmd === "plugin:pylair|python_cancel") return "cancelled";
      if (cmd === "plugin:pylair|python_state") return "ready";
      return null;
    },
    async invokeChannel(cmd, args, onEvent) {
      calls.push({ cmd, args });
      assert.equal(cmd, "plugin:pylair|python_call_stream");
      const task = nextTask++;
      channels.set(task, onEvent);
      return { task, protocol: 2 };
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
const tick = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => _resetForTests());

test("call resolves with the terminal result", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("add", { a: 1, b: 2 });
  await tick(); // let invokeChannel resolve, task=100
  b.emitTask(100, streamDone(100, 3));
  assert.equal(await p, 3);
});

test("old plugin's bare task ack errors loudly and cancels the orphan", async () => {
  const b = fakeBridge();
  // An old python_call_stream ignores `onEvent` and returns a bare task
  // id — the task runs with no channel to report on.
  b.invokeChannel = async (cmd, args) => {
    b.calls.push({ cmd, args });
    return 7;
  };
  setBridge(b);
  await assert.rejects(
    call("add", { a: 1, b: 2 }),
    /older than the client.*tauri-plugin-pylair/,
  );
  await tick();
  const cancels = b.calls.filter((c) => c.cmd === "plugin:pylair|python_cancel");
  assert.deepEqual(
    cancels.map((c) => c.args),
    [{ task: 7 }],
    "the orphaned task must be cancelled",
  );
});

test("stream also rejects an old plugin's bare task ack", async () => {
  const b = fakeBridge();
  b.invokeChannel = async (cmd, args) => {
    b.calls.push({ cmd, args });
    return 9;
  };
  setBridge(b);
  await assert.rejects(
    (async () => {
      for await (const _ of stream("counter", { n: 1 })) void _;
    })(),
    /older than the client.*tauri-plugin-pylair/,
  );
});

test("call rejects with PythonError on runtime error", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("boom");
  await tick();
  b.emitTask(100, streamErr(100, -32000, "kaboom"));
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32000);
});

test("abort signal sends python_cancel for the task", async () => {
  const b = fakeBridge();
  setBridge(b);
  const ac = new AbortController();
  const p = call("slow", {}, { signal: ac.signal });
  await tick();
  ac.abort();
  await tick();
  assert.ok(b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100));
  // runtime then reports TASK_CANCELLED as the terminal
  b.emitTask(100, streamErr(100, -32010, "task cancelled"));
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
  await tick();
  b.emitTask(100, streamChunk(100, 0, "a"));
  b.emitTask(100, streamChunk(100, 1, "b"));
  b.emitTask(100, streamChunk(100, 2, "c"));
  b.emitTask(100, streamDone(100, { done: true }));
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
  await tick();
  b.emitTask(100, streamErr(100, -32031, "runtime crashed"));
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
  await tick();
  for (let s = 0; s < 40; s++) b.emitTask(100, streamChunk(100, s, s));
  await consume;
  const grants = b.calls.filter((c) => c.cmd === "plugin:pylair|python_grant_credit");
  assert.ok(grants.length >= 1 && grants.every((g) => g.args.task === 100));
});

test("each call only ever sees its own channel events", async () => {
  // The channel is bound at invoke time: an event for task 100 can
  // never reach task 101 — there is no shared bus to leak across.
  const b = fakeBridge();
  setBridge(b);
  const a = call("add");
  const c = call("add");
  await tick();
  b.emitTask(100, streamDone(100, "A"));
  b.emitTask(101, streamDone(101, "B"));
  assert.equal(await a, "A");
  assert.equal(await c, "B");
});

test("cancel() forwards task id and returns the outcome", async () => {
  const b = fakeBridge();
  setBridge(b);
  assert.equal(await cancel(77), "cancelled");
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
  await tick();
  assert.deepEqual(b.calls[0].args.input.data, { $b64: "AAEC" });
  assert.deepEqual(b.calls[0].args.input.nested.blob, { $b64: "/w==" });
  // $b64 tags decode on the way back
  b.emitTask(100, streamDone(100, { $b64: "AgEA" }));
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
  await tick();
  b.emitTask(100, streamChunk(100, 0, { $b64: "aGk=" }));
  b.emitTask(100, streamDone(100, { done: true }));
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

/**
 * Bridge whose channel fires events BEFORE the invoke resolves — what
 * the Rust side can do: the forwarder starts sending as soon as Python
 * replies, racing the invoke response that carries the task id. The
 * callback is bound at invoke time, so nothing is lost.
 */
function earlyBridge(eventsFor) {
  const b = fakeBridge();
  const inner = b.invokeChannel.bind(b);
  b.invokeChannel = async (cmd, args, onEvent) => {
    const task = await inner(cmd, args, onEvent);
    if (cmd === "plugin:pylair|python_call_stream") {
      for (const ev of eventsFor(task)) onEvent(ev);
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

test("breaking out of a stream cancels the producer", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("counter", { n: 1000 });
  const first = it.next();
  await tick();
  b.emitTask(100, streamChunk(100, 0, "a"));
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
  await tick();
  b.emitTask(100, streamChunk(100, 0, "a"));
  await first;
  const second = it.next();
  b.emitTask(100, streamDone(100, null));
  assert.equal((await second).done, true);
  assert.ok(!b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});

test("call with timeoutMs rejects with a deadline error and cancels the task", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("slow", {}, { timeoutMs: 50 });
  await assert.rejects(p, (e) => e instanceof PythonError && e.code === -32013);
  assert.ok(
    b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel" && c.args.task === 100),
    "an expired deadline must cancel the producer",
  );
});

test("a call that finishes before its deadline is unaffected", async () => {
  const b = fakeBridge();
  setBridge(b);
  const p = call("fast", {}, { timeoutMs: 500 });
  await tick();
  b.emitTask(100, streamDone(100, "ok"));
  assert.equal(await p, "ok");
  await new Promise((r) => setTimeout(r, 20));
  assert.ok(!b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});

test("stream with timeoutMs throws the deadline error and cancels", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("slow", {}, { timeoutMs: 50 });
  await assert.rejects(it.next(), (e) => e instanceof PythonError && e.code === -32013);
  assert.ok(b.calls.some((c) => c.cmd === "plugin:pylair|python_cancel"));
});

test("invoke rejection with LIMIT_EXCEEDED surfaces LimitExceededError", async () => {
  const b = fakeBridge();
  // Saturated host: python_call_stream rejects with the serialized ErrorObj.
  const realChannel = b.invokeChannel.bind(b);
  b.invokeChannel = async (cmd, args, onEvent) => {
    if (cmd === "plugin:pylair|python_call_stream")
      throw {
        code: -32040,
        message: "too many in-flight tasks",
        data: { limit: "max_concurrent_tasks", max: 64 },
      };
    return realChannel(cmd, args, onEvent);
  };
  setBridge(b);
  await assert.rejects(
    call("anything"),
    (e) =>
      e instanceof LimitExceededError &&
      e.limit === "max_concurrent_tasks" &&
      e.max === 64 &&
      e.code === -32040,
  );
});

test("stream terminal LIMIT_EXCEEDED throws LimitExceededError with .limit", async () => {
  const b = fakeBridge();
  setBridge(b);
  const it = stream("flood");
  const first = it.next(); // generators start on next(): binds the channel, task=100
  await tick();
  b.emitTask(100, {
    task: 100,
    done: true,
    error: {
      code: -32040,
      message: "stream buffer cap",
      data: { limit: "stream_buffer_bytes", max: 16777216 },
    },
  });
  await assert.rejects(
    first,
    (e) =>
      e instanceof LimitExceededError &&
      e.limit === "stream_buffer_bytes" &&
      e.code === -32040,
  );
});
