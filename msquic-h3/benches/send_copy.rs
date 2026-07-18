//! INFORMATIONAL send-copy measurements (MF-4 / FR-012 ceiling inputs).
//!
//! This Criterion target is a separate crate, so it reaches the copy path only
//! through the gated public entry point `msquic_h3::bench_support::copy_into_send_buffer`
//! (compiled in **only** under the committed `bench-internals` feature — see the
//! `[[bench]]` `required-features` in `Cargo.toml`). `&[u8]` is a `Buf`, so this
//! drives the SAME `src.copy_to_bytes(remaining)` allocation production runs on
//! `&mut WriteBuf<B>`.
//!
//! Three measurements, ALL informational (no hard fail here — the BLOCKING,
//! deterministic memory gate is the in-crate `send_copy_peak_resident_bound`
//! `#[test]`):
//!
//! 1. **Per-send copy cost** (Criterion): the isolated wall-time of ONE owned
//!    copy at representative payload sizes.
//! 2. **Aggregate send-buffer memory under concurrent streams**: hold N owned
//!    `SendBuffer`s simultaneously (N × up-to-ceiling payloads, mirroring N
//!    concurrent outstanding streams) and record the aggregate *retained* heap
//!    bytes / peak. This demonstrates the design's unbounded-aggregate concern:
//!    retained memory grows linearly in the number of in-flight sends with no
//!    adapter-level ceiling on the sum. Feeds the FR-012 provisional-ceiling
//!    decision (recorded, not ratified here).
//! 3. **Scheduler/executor latency of the synchronous copy**: the wall-time the
//!    synchronous copy blocks the calling task at representative payload sizes
//!    (the copy runs inline on the caller's thread, so this time is the latency
//!    it injects into the send executor before the native submit).
//!
//! Timing here is **informational / relative to a same-run baseline** — there is
//! NO hard 10 ms assertion (that number stays advisory until a pinned, isolated
//! runner exists).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use criterion::{Criterion, black_box};
use msquic_h3::bench_support::copy_into_send_buffer;

// ── Bench-local counting allocator ──────────────────────────────────────────
// Installed ONLY for this benchmark binary (never the library), it counts live
// and peak heap bytes on threads that opted in via `measure_retained`, so the
// aggregate-memory measurement is not polluted by Criterion's own allocations.
thread_local! {
    // `const` init is allocation-free, so reading it from inside the global
    // allocator cannot re-enter the allocator.
    static MEASURING: Cell<bool> = const { Cell::new(false) };
}

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

#[inline]
fn measuring() -> bool {
    MEASURING.try_with(|m| m.get()).unwrap_or(false)
}

// SAFETY: forwards every request to the system allocator unchanged; the only
// added work is bookkeeping on process-global atomics, and it never returns a
// different pointer than `System` produced.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() && measuring() {
            let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if measuring() {
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        unsafe { System.dealloc(p, l) };
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Run `op` with this-thread heap counting enabled and return
/// `(op_result, live_delta, peak_delta)` where the deltas are bytes attributable
/// to the op (baseline excluded). The result is returned so the caller can keep
/// the allocations alive across the read.
fn measure_retained<R>(op: impl FnOnce() -> R) -> (R, usize, usize) {
    MEASURING.with(|m| m.set(true));
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let out = op();
    let live = LIVE.load(Ordering::SeqCst);
    let peak = PEAK.load(Ordering::SeqCst);
    MEASURING.with(|m| m.set(false));
    (
        out,
        live.saturating_sub(baseline),
        peak.saturating_sub(baseline),
    )
}

// ── Measurement 1: per-send copy cost (Criterion) ───────────────────────────
fn bench_send_copy(c: &mut Criterion) {
    // Payload sizes: header-only (~32 B), small body (1 KiB), medium (64 KiB), and
    // large contiguous bodies up to `MAX_ADAPTER_SEND` (16 MiB).
    for &size in &[32usize, 1 << 10, 64 << 10, 1 << 20, 8 << 20, 16 << 20] {
        // Source built ONCE, OUTSIDE the timed region, so Criterion times ONLY the
        // owned copy.
        let src: Vec<u8> = vec![0u8; size];
        c.bench_function(&format!("copy_{size}"), |b| {
            b.iter(|| {
                let dst = copy_into_send_buffer(black_box(&src[..])); // TIMED: copy only
                black_box(&dst);
            });
        });
    }
}

// ── Measurement 2: aggregate send-buffer memory under concurrent streams ────
/// Hold N owned `SendBuffer`s at once (each a `per_stream`-byte payload, up to
/// the 16 MiB ceiling) to model N concurrent outstanding streams, and report the
/// aggregate *retained* heap bytes while all N are live. Because nothing caps the
/// SUM of outstanding owned copies, retained memory grows linearly in N — the
/// unbounded-aggregate concern the design flags and FR-012 must bound.
fn measure_aggregate_memory() {
    println!("\n=== aggregate send-buffer memory under concurrent streams (informational) ===");
    println!("(each outstanding stream retains one owned SendBuffer of `per_stream` bytes;");
    println!(" the adapter imposes NO ceiling on the SUM, so retained memory is unbounded in N)");

    // A representative per-stream payload (256 KiB) plus a near-ceiling payload
    // (16 MiB) so the linear, unbounded growth is visible at both scales.
    for &per_stream in &[256usize << 10, 16usize << 20] {
        println!(
            "\n  per-stream payload = {per_stream} bytes ({} KiB):",
            per_stream >> 10
        );
        for &n in &[1usize, 8, 64, 256] {
            // Build the source OUTSIDE the measured region so the measurement
            // captures only the RETAINED owned copies, not the transient source.
            let src: Vec<u8> = vec![0u8; per_stream];
            let (held, live, peak) = measure_retained(|| {
                let mut bufs = Vec::with_capacity(n);
                for _ in 0..n {
                    // One owned SendBuffer per simulated outstanding stream; all N
                    // are retained in `bufs` simultaneously, exactly as N in-flight
                    // sends each hold their box until SendComplete.
                    bufs.push(copy_into_send_buffer(black_box(&src[..])));
                }
                bufs
            });
            let expected = (n as u64) * (per_stream as u64);
            println!(
                "    N={n:>4} outstanding -> retained {live:>12} B, peak {peak:>12} B \
                 (~N*payload = {expected} B)",
            );
            // Keep the N buffers alive until AFTER the read above, then drop.
            drop(held);
        }
    }
}

// ── Measurement 3: scheduler/executor latency of the synchronous copy ───────
/// Time the synchronous copy inline (as it runs on the caller's send-executor
/// task) at representative payload sizes and report per-call wall-time. This is
/// the latency the synchronous copy injects before the native submit; it stays
/// INFORMATIONAL (no hard fail on an unpinned runner).
fn measure_copy_latency() {
    println!("\n=== synchronous-copy scheduler/executor latency (informational) ===");
    println!("(wall-time the inline copy blocks the calling send task, per payload size)");
    for &size in &[1usize << 10, 64 << 10, 1 << 20, 8 << 20, 16 << 20] {
        let src: Vec<u8> = vec![0u8; size];
        // Warm up so first-touch page faults / allocator arena growth don't skew
        // the reported figure.
        for _ in 0..8 {
            black_box(copy_into_send_buffer(black_box(&src[..])));
        }
        let iters: u32 = if size >= (8 << 20) { 50 } else { 500 };
        // Capture elapsed immediately after each copy, BEFORE the buffer is
        // dropped, so the reported figure isolates the synchronous copy and does
        // not include SendBuffer destruction/deallocation.
        let mut total = std::time::Duration::ZERO;
        for _ in 0..iters {
            let t = Instant::now();
            let dst = copy_into_send_buffer(black_box(&src[..]));
            total += t.elapsed();
            black_box(&dst);
            // `dst` drops here, outside the measured interval.
        }
        let per_call = total / iters;
        println!(
            "  size {size:>9} B ({:>5} KiB): {per_call:>12?}/copy over {iters} iters",
            size >> 10
        );
    }
}

fn main() {
    // Print the informational aggregate-memory + latency measurements first so
    // they appear in the CI log and can be cited in Phase-10 Development.md, then
    // hand off to Criterion for the per-send copy-cost timing.
    measure_aggregate_memory();
    measure_copy_latency();

    let mut criterion = Criterion::default().configure_from_args();
    bench_send_copy(&mut criterion);
    criterion.final_summary();
}
