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

async function ensureStreamListener(): Promise<void> {
  streamListener ??= (await ipc()).listen<StreamEvent>(EVT.stream, (ev) => {
    tasks.get(ev.task)?.onItem(ev);
  });
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
}

/** Call a Python function; resolves with its result. */
export async function call<T = unknown>(
  fn: string,
  input: Record<string, unknown> = {},
  opts: CallOptions = {},
): Promise<T> {
  const b = await ipc();
  await ensureStreamListener();
  const task = await b.invoke<number>(CMD.callStream, {
    fnName: fn,
    input: toWire(input) as Record<string, unknown>,
  });
  wireAbort(opts.signal, task);
  return new Promise<T>((resolve, reject) => {
    tasks.set(task, {
      onItem: (ev) => {
        if (!ev.done) return; // plain calls shouldn't chunk; ignore defensively
        tasks.delete(task);
        if (ev.error) reject(new PythonError(ev.error.code, ev.error.message, ev.error.data));
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
  const task = await b.invoke<number>(CMD.callStream, {
    fnName: fn,
    input: toWire(input) as Record<string, unknown>,
  });
  wireAbort(opts.signal, task);

  const queue: StreamEvent[] = [];
  let wake: (() => void) | null = null;
  let pendingCredit = 0;

  tasks.set(task, {
    onItem: (ev) => {
      queue.push(ev);
      wake?.();
    },
  });
  try {
    for (;;) {
      while (queue.length === 0) {
        await new Promise<void>((r) => (wake = r));
        wake = null;
      }
      const ev = queue.shift()!;
      if (ev.done) {
        if (ev.error) throw new PythonError(ev.error.code, ev.error.message, ev.error.data);
        return;
      }
      // Pull-flow: top up credit in batches as the consumer pulls.
      if (++pendingCredit >= CREDIT_BATCH) {
        void b.invoke(CMD.grantCredit, { task, n: pendingCredit });
        pendingCredit = 0;
      }
      yield fromWire(ev.value) as T;
    }
  } finally {
    tasks.delete(task);
    if (pendingCredit > 0) void b.invoke(CMD.grantCredit, { task, n: pendingCredit });
  }
}

/** Explicitly cancel a running task by id. */
export async function cancel(task: number): Promise<boolean> {
  return (await ipc()).invoke<boolean>(CMD.cancel, { task });
}

/** Retry after the supervisor reaches `failed` — respawns the runtime. */
export async function retry(): Promise<void> {
  await (await ipc()).invoke(CMD.retry);
}

/** Current supervisor state. */
export async function state(): Promise<RuntimeState> {
  return (await ipc()).invoke<RuntimeState>(CMD.state);
}

/** Ask the supervisor to shut the runtime down cleanly. */
export async function shutdown(): Promise<void> {
  await (await ipc()).invoke(CMD.shutdown);
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
  streamListener = null;
  _bridge = null;
  _defaultBridge = null;
}
