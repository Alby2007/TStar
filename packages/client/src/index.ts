/**
 * @tauri-plus/client — typed calls into the managed Python runtime.
 *
 * Stub until M2 (typed bridge). The generated `python` client will be
 * produced by `tauri-plus codegen` from the Python schema; see
 * PROJECT_BRIEF.md sections 4-5.
 */

/** Opaque handle to a running Python task; used for cancellation. */
export interface PythonCallOptions {
  signal?: AbortSignal;
}

/**
 * Placeholder for the generated, typed `python.<fn>()` client.
 * Real shape is defined by `tauri-plus codegen` output in the app project.
 */
export const python: Record<string, never> = {};
