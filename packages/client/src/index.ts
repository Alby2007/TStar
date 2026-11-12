/**
 * @pylair/client — typed calls into the managed Python runtime.
 *
 * Transport: `plugin:pylair|python_*` commands + `pylair://*` events
 * (see crates/tauri-plugin-pylair/src/lib.rs `events` and
 * docs/protocol-v1.md). The bridge is injectable for testing/headless use.
 */

/** Minimal bridge over Tauri invoke/listen — injectable for tests. */
export interface IpcBridge {
  invoke<T = unknown>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  listen<T = unknown>(event: string, cb: (payload: T) => void): Promise<() => void>;
}

/** Python-side error object (docs/protocol-v1.md §8). */
export class PythonError extends Error {
  readonly code: number;
  readonly data?: unknown;
  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = "PythonError";
    this.code = code;
    this.data = data;
  }
}

/** Protocol code for limit rejections (docs/protocol-v1.md §8). */
export const LIMIT_EXCEEDED = -32040;

/**
 * A resource limit was hit — the host or runtime refused work rather than
 * queueing it unboundedly. `.limit` names the limit
 * ("max_concurrent_tasks", "max_concurrent_streams", "max_queued_commands",
 * "stream_buffer_bytes", "stream_buffer_bytes_total", "max_stream_credit",
 * "max_frame", "max_isolated_workers", "frame_out_queue"), `.max` carries
 * the configured bound when the host reported one.
 */
export class LimitExceededError extends PythonError {
  readonly limit: string;
  readonly max?: number;
  constructor(message: string, data?: unknown) {
    super(LIMIT_EXCEEDED, message, data);
    this.name = "LimitExceededError";
    const d = (data ?? {}) as { limit?: unknown; max?: unknown };
    this.limit = typeof d.limit === "string" ? d.limit : "unknown";
    if (typeof d.max === "number") this.max = d.max;
  }
}

/** Build the typed error for a wire code. */
function mkError(code: number, message: string, data?: unknown): PythonError {
  return code === LIMIT_EXCEEDED
    ? new LimitExceededError(message, data)
    : new PythonError(code, message, data);
}

/**
 * Normalize a thrown value into PythonError IF it is a serialized error
 * object (`{code, message, data?}`); anything else passes through
 * unchanged — transport failures keep their original identity.
 */
function asPythonError(e: unknown): unknown {
  const o = e as { code?: unknown; message?: unknown; data?: unknown };
  if (o && typeof o.code === "number")
    return mkError(
      o.code,
      typeof o.message === "string" ? o.message : String(e),
      o.data,
    );
  return e;
}

export type RuntimeState =
  | "starting"
  | "ready"
  | "unresponsive"
  | "restarting"
  | "failed"
  | "stopped";

export interface StateChange {
  from: RuntimeState;
  to: RuntimeState;
  detail: string;
}

export interface StreamEvent {
  task: number;
  seq?: number;
  value?: unknown;
  done: boolean;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

const EVT = {
  state: "pylair://state",
  stderr: "pylair://stderr",
  exposes: "pylair://exposes",
  stream: "pylair://stream",
} as const;

const CMD = {
  selftestReport: "plugin:pylair|python_selftest_report",
  call: "plugin:pylair|python_call",
  callStream: "plugin:pylair|python_call_stream",
  grantCredit: "plugin:pylair|python_grant_credit",
  cancel: "plugin:pylair|python_cancel",
  retry: "plugin:pylair|python_retry",
  state: "plugin:pylair|python_state",
  shutdown: "plugin:pylair|python_shutdown",
} as const;

// ---- bridge management -----------------------------------------------------

let _bridge: IpcBridge | null = null;
let _defaultBridge: Promise<IpcBridge> | null = null;

function defaultBridge(): Promise<IpcBridge> {
  _defaultBridge ??= Promise.all([
    import("@tauri-apps/api/core"),
    import("@tauri-apps/api/event"),
  ]).then(([core, event]): IpcBridge => {
    return {
      invoke: (cmd, args) => core.invoke(cmd, args),
      listen: <T,>(ev: string, cb: (p: T) => void) =>
        event.listen<T>(ev, (e) => cb(e.payload)),
    };
  });
  return _defaultBridge;
}

/** Inject a custom bridge (tests, non-webview hosts). */
export function setBridge(bridge: IpcBridge | null): void {
  _bridge = bridge;
}

async function ipc(): Promise<IpcBridge> {
  return _bridge ?? defaultBridge();
}

// ---- wire JSON (protocol §2, docs/codegen.md) -------------------------------
//
// The runtime encodes bytes as {"$b64": "<base64>"} — anywhere in the
// payload. `call`/`stream` apply the transform here so generated stubs
// and hand-written callers stay wire-agnostic: pass Uint8Array, get
// Uint8Array back.

const B64_TAG = "$b64";

function b64encode(u8: Uint8Array): string {
  let s = "";
  for (let i = 0; i < u8.length; i += 0x8000)
    s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
  return btoa(s);
}

function b64decode(b64: string): Uint8Array {
  const s = atob(b64);
  const u8 = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) u8[i] = s.charCodeAt(i);
  return u8;
}

function toWire(v: unknown): unknown {
  if (v instanceof Uint8Array) return { [B64_TAG]: b64encode(v) };
  if (Array.isArray(v)) return v.map(toWire);
  if (v !== null && typeof v === "object")
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, x]) => [
        k,
        toWire(x),
      ]),
    );
  return v;
}

function fromWire(v: unknown): unknown {
  if (v !== null && typeof v === "object") {
    const ks = Object.keys(v);
    if (ks.length === 1 && ks[0] === B64_TAG)
      return b64decode((v as Record<string, string>)[B64_TAG]);
    if (Array.isArray(v)) return v.map(fromWire);
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, x]) => [
        k,
        fromWire(x),
      ]),
    );
  }
  return v;
}

// ---- internal stream machinery ---------------------------------------------

/**
 * Every call — plain or streaming — goes through `python_call_stream` so it
 * always has a task id (needed for cancellation). Plain calls simply see a
 * single terminal stream event.
 */
interface ActiveTask {
  onItem: (ev: StreamEvent) => void;
}

const tasks = new Map<number, ActiveTask>();
let streamListener: Promise<() => void> | null = null;

/**
 * Events for a task id nobody has registered yet. The Rust forwarder starts
 * emitting as soon as Python replies, which can beat the `invoke` response
 * that carries the task id back to JS. Every task here was started by this
 * client, so an unknown id is an in-flight invoke: hold its events and
 * replay them on registration. Entries expire so a cancelled/abandoned
 * invoke cannot leak.
 */
const EARLY_TTL_MS = 30_000;
const early = new Map<number, { events: StreamEvent[]; timer: ReturnType<typeof setTimeout> }>();

function holdEarly(ev: StreamEvent): void {
  let e = early.get(ev.task);
  if (!e) {
    const timer = setTimeout(() => early.delete(ev.task), EARLY_TTL_MS);
    (timer as { unref?: () => void }).unref?.();
    e = { events: [], timer };
    early.set(ev.task, e);
  }
  e.events.push(ev);
}

/** Register a task and replay anything that arrived before it existed. */
function register(task: number, t: ActiveTask): void {
  tasks.set(task, t);
  const e = early.get(task);
  if (!e) return;
  early.delete(task);
  clearTimeout(e.timer);
  for (const ev of e.events) {
    t.onItem(ev);
    if (ev.done) break;
  }
}

async function ensureStreamListener(): Promise<void> {
  streamListener ??= (async () =>
    (await ipc()).listen<StreamEvent>(EVT.stream, (ev) => {
      const t = tasks.get(ev.task);
      if (t) t.onItem(ev);
      else holdEarly(ev);
    }))();
  await streamListener;
}

function wireAbort(signal: AbortSignal | undefined, task: number): void {
  if (!signal) return;
  const cancel = () => {
    void ipc().then((b) => b.invoke(CMD.cancel, { task }));
  };
  if (signal.aborted) return cancel();
  signal.addEventListener("abort", cancel, { once: true });
}

// ---- public API -------------------------------------------------------------

export interface CallOptions {
  signal?: AbortSignal;
  /**
   * Application-level deadline. When it expires the task is cancelled and the
   * call rejects/throws a PythonError with code DEADLINE_EXCEEDED, without
   * waiting for the runtime to acknowledge — a blocked runtime cannot be
   * relied on to answer.
   */
  timeoutMs?: number;
}

/** Client-side error code for an expired `timeoutMs`. */
export const DEADLINE_EXCEEDED = -32013;

function deadlineError(ms: number): PythonError {
  return new PythonError(DEADLINE_EXCEEDED, `deadline of ${ms}ms exceeded`);
}

/** Arm `timeoutMs`: on expiry cancel the task and run `onExpire`. */
function armDeadline(
  opts: CallOptions,
  task: number,
  onExpire: (e: PythonError) => void,
): () => void {
  const ms = opts.timeoutMs;
  if (ms === undefined || ms <= 0) return () => {};
  const t = setTimeout(() => {
    void ipc().then((b) => b.invoke(CMD.cancel, { task })).catch(() => {});
    onExpire(deadlineError(ms));
  }, ms);
  (t as { unref?: () => void }).unref?.();
  return () => clearTimeout(t);
}

/** Call a Python function; resolves with its result. */
export async function call<T = unknown>(
  fn: string,
  input: Record<string, unknown> = {},
  opts: CallOptions = {},
): Promise<T> {
  const b = await ipc();
  await ensureStreamListener();
  const task = await b
    .invoke<number>(CMD.callStream, {
      fnName: fn,
      input: toWire(input) as Record<string, unknown>,
    })
    .catch((e) => {
      throw asPythonError(e);
    });
  wireAbort(opts.signal, task);
  return new Promise<T>((resolve, reject) => {
    const disarm = armDeadline(opts, task, (e) => {
      tasks.delete(task);
      reject(e);
    });
    register(task, {
      onItem: (ev) => {
        if (!ev.done) return; // plain calls shouldn't chunk; ignore defensively
        disarm();
        tasks.delete(task);
        if (ev.error) reject(mkError(ev.error.code, ev.error.message, ev.error.data));
        else resolve(fromWire(ev.result) as T);
      },
    });
  });
}

const CREDIT_BATCH = 32;

/** Call an async-generator function; yields chunks as they arrive. */
export async function* stream<T = unknown>(
  fn: string,
  input: Record<string, unknown> = {},
  opts: CallOptions = {},
): AsyncGenerator<T, void, void> {
  const b = await ipc();
  await ensureStreamListener();
  const task = await b
    .invoke<number>(CMD.callStream, {
      fnName: fn,
      input: toWire(input) as Record<string, unknown>,
    })
    .catch((e) => {
      throw asPythonError(e);
    });
  wireAbort(opts.signal, task);

  const queue: StreamEvent[] = [];
  let wake: (() => void) | null = null;
  let pendingCredit = 0;
  let finished = false; // terminal event seen: producer already done
  let expired: PythonError | null = null;
  const disarm = armDeadline(opts, task, (e) => {
    expired = e;
    finished = true; // the deadline already sent the cancel
    wake?.();
  });

  register(task, {
    onItem: (ev) => {
      queue.push(ev);
      wake?.();
    },
  });
  try {
    for (;;) {
      while (queue.length === 0) {
        if (expired) throw expired;
        await new Promise<void>((r) => (wake = r));
        wake = null;
      }
      if (expired) throw expired;
      const ev = queue.shift()!;
      if (ev.done) {
        finished = true;
        if (ev.error) throw mkError(ev.error.code, ev.error.message, ev.error.data);
        return;
      }
      // Pull-flow: top up credit in batches as the consumer pulls.
      if (++pendingCredit >= CREDIT_BATCH) {
        // Grant can now reject loudly (task finished between grants —
        // TASK_NOT_FOUND). A rejected grant is unremarkable: the stream
        // is already over, so swallow it.
        void b.invoke(CMD.grantCredit, { task, n: pendingCredit }).catch(() => {});
        pendingCredit = 0;
      }
      yield fromWire(ev.value) as T;
    }
  } finally {
    disarm();
    tasks.delete(task);
    // Consumer walked away (break / return() / throw) before the producer
    // finished: stop it instead of leaving it running and granting credit.
    if (!finished) void b.invoke(CMD.cancel, { task }).catch(() => {});
    else if (pendingCredit > 0)
      void b.invoke(CMD.grantCredit, { task, n: pendingCredit }).catch(() => {});
  }
}

/**
 * What a cancel actually did — distinct outcomes, never an ambiguous bool.
 * `cancelled`: runtime confirmed the work stopped.
 * `requested_may_continue`: inline sync work can't be interrupted — the
 *   caller sees TASK_CANCELLED but the Python may run to completion.
 * `terminated`: an isolated worker was terminated and reaped.
 * `termination_uncertain`: termination was requested but unconfirmed —
 *   the work may still be running.
 * `completed`: the task had already finished before the cancel landed.
 * `not_found`: no such task in this runtime generation (stale id).
 * `rejected`: this webview does not own the task.
 * `delivery_failed`: the control frame could not be queued at all.
 */
export type CancelOutcome =
  | "cancelled"
  | "requested_may_continue"
  | "terminated"
  | "termination_uncertain"
  | "completed"
  | "not_found"
  | "rejected"
  | "delivery_failed";

/** What shutdown actually achieved — reported, never assumed. */
export type ShutdownOutcome =
  | "terminated" // graceful exit inside the grace window
  | "forced_kill" // had to SIGKILL/taskkill — the tree is confirmed dead
  | "incomplete" // kill sent but the exit wasn't confirmed
  | "unknown"; // runtime already gone or state unknown

/** Explicitly cancel a running task by id. Only the task's own webview
 * may cancel it — the supervisor enforces ownership server-side. */
export async function cancel(task: number): Promise<CancelOutcome> {
  return (await ipc()).invoke<CancelOutcome>(CMD.cancel, { task });
}

/** Retry after the supervisor reaches `failed` — respawns the runtime. */
export async function retry(): Promise<void> {
  await (await ipc()).invoke(CMD.retry);
}

/** Current supervisor state. */
export async function state(): Promise<RuntimeState> {
  return (await ipc()).invoke<RuntimeState>(CMD.state);
}

/** Ask the supervisor to shut the runtime down; resolves to what
 * cleanup actually achieved, not merely that the request was queued. */
export async function shutdown(): Promise<ShutdownOutcome> {
  return (await ipc()).invoke<ShutdownOutcome>(CMD.shutdown);
}

/** Subscribe to lifecycle transitions; returns an unlisten function. */
export async function onState(cb: (s: StateChange) => void): Promise<() => void> {
  return (await ipc()).listen<StateChange>(EVT.state, cb);
}

/** Subscribe to runtime stderr lines. */
export async function onStderr(cb: (line: string) => void): Promise<() => void> {
  return (await ipc()).listen<string>(EVT.stderr, cb);
}

/** Subscribe to the exposes manifest emitted after handshake. */
export async function onExposes<T = unknown>(cb: (manifest: T) => void): Promise<() => void> {
  return (await ipc()).listen<T>(EVT.exposes, cb);
}

// ---- installed-webview self-test --------------------------------------------

export interface SelfTestSpec {
  /** Function returning a number: add(a, b). */
  add: string;
  /** Async generator yielding 0..n-1 slowly. */
  count: string;
}

/**
 * When the host runs the app with PYLAIR_SELF_TEST_WEBVIEW, the plugin sets
 * `window.__PYLAIR_SELF_TEST__`. Call this once at startup: it drives the
 * REAL bridge (ACL permissions, event routing, wire transform) — a typed
 * call, an ordered stream, cancellation, shutdown — then reports the verdict
 * so the host process exits 0/5. A no-op in normal runs.
 */
export async function maybeSelfTest(
  spec: SelfTestSpec = { add: "add", count: "count" },
): Promise<void> {
  if (!(globalThis as { __PYLAIR_SELF_TEST__?: boolean }).__PYLAIR_SELF_TEST__) return;
  const checks: Record<string, string> = {};
  let ok = true;
  const run = async (name: string, f: () => Promise<void>) => {
    try {
      await f();
      checks[name] = "ok";
    } catch (e) {
      ok = false;
      checks[name] = `FAIL: ${e instanceof Error ? e.message : String(e)}`;
    }
  };
  const within = <T,>(p: Promise<T>, ms: number, what: string) =>
    Promise.race([
      p,
      new Promise<T>((_, rej) => setTimeout(() => rej(new Error(`${what} timed out`)), ms)),
    ]);

  await run("call", async () => {
    const v = await within(call<number>(spec.add, { a: 2, b: 3 }), 30_000, "call");
    if (v !== 5) throw new Error(`add(2,3) returned ${String(v)}`);
  });
  await run("stream-order", async () => {
    const got: number[] = [];
    await within(
      (async () => {
        for await (const n of stream<number>(spec.count, { n: 5 })) got.push(n);
      })(),
      30_000,
      "stream",
    );
    if (got.join(",") !== "0,1,2,3,4") throw new Error(`stream gave [${got.join(",")}]`);
  });
  await run("cancel", async () => {
    const ac = new AbortController();
    let seen = 0;
    let code: number | undefined;
    await within(
      (async () => {
        try {
          for await (const _ of stream<number>(spec.count, { n: 1000 }, { signal: ac.signal })) {
            if (++seen === 1) ac.abort();
          }
        } catch (e) {
          if (e instanceof PythonError) code = e.code;
          else throw e;
        }
      })(),
      30_000,
      "cancel",
    );
    if (code !== -32010) throw new Error(`expected TASK_CANCELLED, got ${String(code)} after ${seen} chunks`);
    if (seen >= 100) throw new Error(`producer kept running (${seen} chunks)`);
  });
  await (await ipc()).invoke(CMD.selftestReport, { result: { ok, checks } });
}

/**
 * Namespace-style convenience surface; `python.<fn>` typed wrappers are
 * generated by `pylair codegen` in M2 on top of call/stream.
 */
export const python = {
  call,
  stream,
  cancel,
  retry,
  state,
  shutdown,
  onState,
  onStderr,
  onExposes,
};

/** For tests: reset all listeners/tasks between cases. */
export function _resetForTests(): void {
  tasks.clear();
  for (const e of early.values()) clearTimeout(e.timer);
  early.clear();
  streamListener = null;
  _bridge = null;
  _defaultBridge = null;
}
