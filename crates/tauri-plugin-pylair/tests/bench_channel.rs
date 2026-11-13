//! Throughput comparison for the channel migration — `#[ignore]`d, run
//! manually with `cargo test --test bench_channel -- --ignored --nocapture`.
//!
//! Measures the per-payload delivery machinery of both paths at full
//! speed (a real Python producer frames ~1k chunks/s, which would bury
//! the hop cost in production time):
//!
//!   old (approx): serialize + trigger-JS-string build + listener
//!                 dispatch — the in-Rust work `emit_to` does per item.
//!                 The label lookup and `webview.eval` it ends in are
//!                 common to the channel path (`Channel::send` is an
//!                 eval too) and excluded from both.
//!   new:          `Channel::send` — serialize + on-message dispatch.

use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tauri::ipc::{Channel, InvokeResponseBody};

fn payload(task: u64, seq: u64, value: &serde_json::Value) -> serde_json::Value {
    json!({"task": task, "seq": seq, "value": value, "done": false})
}

#[test]
#[ignore = "throughput bench — run manually"]
fn bench_delivery_paths() {
    // ---- 100k small chunks ----
    const N: usize = 100_000;
    let small = serde_json::Value::from(42);

    let bytes = Arc::new(AtomicU64::new(0));
    let b = bytes.clone();
    let ch = Channel::<serde_json::Value>::new(move |body: InvokeResponseBody| {
        let InvokeResponseBody::Json(s) = body else {
            panic!("Raw channel body");
        };
        b.fetch_add(s.len() as u64, Ordering::Relaxed);
        Ok(())
    });

    let t0 = Instant::now();
    for seq in 0..N as u64 {
        ch.send(payload(1, seq, &small)).unwrap();
    }
    let t_new_small = t0.elapsed();

    let listener_bytes = AtomicU64::new(0);
    let listener = |js: &str| {
        listener_bytes.fetch_add(js.len() as u64, Ordering::Relaxed);
    };
    let t0 = Instant::now();
    for seq in 0..N as u64 {
        let js = format!(
            "window.__TAURI_INTERNALS__.trigger('pylair://stream', {})",
            serde_json::to_string(&payload(1, seq, &small)).unwrap()
        );
        listener(&js);
    }
    let t_old_small = t0.elapsed();

    println!(
        "small chunks ({N}):\n  OLD emit (rust-side): {t_old_small:?} → {:.0}/s\n  NEW channel:          {t_new_small:?} → {:.0}/s\n  delta: {:+.1}%",
        N as f64 / t_old_small.as_secs_f64(),
        N as f64 / t_new_small.as_secs_f64(),
        (t_new_small.as_secs_f64() / t_old_small.as_secs_f64() - 1.0) * 100.0,
    );

    // ---- a few large chunks ----
    for kb in [64usize, 256, 1024] {
        let big = serde_json::Value::from("x".repeat(kb * 1024));
        let seqs = 0..64u64;

        let t0 = Instant::now();
        for seq in seqs.clone() {
            ch.send(payload(1, seq, &big)).unwrap();
        }
        let t_new = t0.elapsed();

        let t0 = Instant::now();
        for seq in seqs {
            let js = format!(
                "window.__TAURI_INTERNALS__.trigger('pylair://stream', {})",
                serde_json::to_string(&payload(1, seq, &big)).unwrap()
            );
            listener(&js);
        }
        let t_old = t0.elapsed();

        println!(
            "{kb} KiB chunks (64):\n  OLD emit (rust-side): {t_old:?} → {:.0}/s\n  NEW channel:          {t_new:?} → {:.0}/s\n  delta: {:+.1}%",
            64.0 / t_old.as_secs_f64(),
            64.0 / t_new.as_secs_f64(),
            (t_new.as_secs_f64() / t_old.as_secs_f64() - 1.0) * 100.0,
        );
    }
}
