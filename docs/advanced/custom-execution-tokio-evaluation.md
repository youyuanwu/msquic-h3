# Evaluation: msquic Custom Execution + Tokio Integration for `msquic-h3`

**Question:** Should `msquic-h3` adopt msquic's **custom execution** feature
(`ExecutionCreate` / `ExecutionPoll` / `ExecutionDelete`, `QUIC_EXECUTION_CONFIG`,
`QUIC_EVENTQ`) — letting the application own the threads/event queue msquic runs
on — and integrate that execution loop with **tokio**, instead of letting msquic
manage its own worker threads?

**Recommendation: DO NOT adopt now. Defer / effectively reject** until the
preconditions in [§8](#8-conditions-to-revisit) hold. The one unique payoff —
eliminating msquic's own ~1‑thread‑per‑processor worker pool in a Tokio‑native
service — is niche and unquantified, while the cost is: a **preview** msquic API
with no safe Rust wrapper, coupling to msquic's **internal** per‑platform
submission‑entry ABI, running msquic protocol work + callbacks **inline on Tokio
workers** (blocking / re‑entrancy / teardown‑deadlock hazards), a Linux‑only
integration path (Windows IOCP and macOS bindings don't fit), and the regression
of the crate's deliberate **runtime‑agnostic** design. The default per‑core thread
footprint that motivates it (§4) is bounded far more cheaply by capping/pinning
the worker pool via `QUIC_GLOBAL_EXECUTION_CONFIG` — not by a custom event loop
(§7). And if the full integration is ever pursued, it belongs in a **separate
`msquic-tokio` crate**, not in `msquic-h3` (§10).

Method: code research over the crate + vendored msquic + tokio public API, plus a
two‑perspective review (integration advocate — `gpt-5.6-sol`; feasibility/safety
skeptic — `claude-opus-4.8`), each grounding claims in `file:line`.

---

## 1. The two execution models

**Default (what the crate uses today).** msquic creates its own worker threads
(≈ one per processor, NUMA/RSS‑aligned) and invokes all object callbacks on those
threads. Each connection + its streams is effectively single‑threaded; listener
callbacks fire in parallel across processors (msquic `docs/Execution.md`,
"Threading").

**Custom execution.** The application creates a platform event queue —
**epoll fd** on Linux, **IOCP** on Windows, **kqueue** on macOS — and one or more
execution *contexts* bound to it. msquic then creates **no** threads of its own;
the app must drive each context by looping: `ExecutionPoll(ctx)` returns a
wait‑time (ms to the next timer), the app drains the platform completion queue,
and calls each completion entry's handler (`Sqe->Completion(cqe)`). Because msquic
and the app share one queue, "the application must use the same base format for
its own submission entries" (`CXPLAT_SQE`) — i.e. couple to msquic's internal
cross‑platform (cxplat) ABI (`docs/Execution.md`, "Custom Execution").

## 2. How `msquic-h3` bridges to async today

The library core depends only on `futures` (`Cargo.toml:38`); **`tokio` is a
dev‑dependency only** (`Cargo.toml:69`). msquic runs callbacks on its own threads;
those callbacks push events into `futures::mpsc::unbounded` channels
(`stream.rs:490-617,684,834-929`; `connection.rs`), and *any* async runtime polls
those channels, woken by the channel's `Waker`. `registration-lifecycle.md` states
the teardown machinery uses "only futures/std so the adapter stays
executor‑agnostic (no tokio dependency)." So there is one msquic‑worker‑thread →
channel/waker → runtime‑task handoff per event, and the crate works on any
executor.

## 3. Can tokio actually drive msquic's execution? (feasibility of the integration)

- **Linux — yes, with care.** Give msquic an epoll fd, wrap it in
  `tokio::io::unix::AsyncFd` (registers an arbitrary Unix fd with tokio's
  reactor), and drive `ExecutionPoll` + drain from a tokio task. Two subtleties
  that are easy to get wrong:
  - **Readiness alone is insufficient.** `ExecutionPoll` returns a *timer*
    wait‑time; timers do **not** make the epoll fd readable. The driver must race
    `AsyncFd::readable()` against `tokio::time::sleep(wait_ms)` (treating
    `UINT32_MAX` as "no timer"), or it will miss timer‑driven work.
  - **Edge‑trigger discipline.** tokio's `AsyncFd` is edge‑triggered; you must
    fully drain `epoll_wait(..., timeout=0)` before `clear_ready()`, or wake‑ups
    are lost / it busy‑spins.
- **Windows — no.** `QUIC_EVENTQ` is an IOCP HANDLE; tokio exposes `AsyncFd`
  only on Unix and does not expose its reactor's IOCP for sharing. Integration
  would need a separate blocking completion‑drain thread — defeating the purpose.
- **macOS — blocked today.** Conceptually kqueue‑in‑kqueue works, but the msquic
  Rust crate currently **reuses the Linux bindings on macOS** as an explicit hack
  (`src/rs/ffi/mod.rs`: "TODO: macos currently is using the linux bindings"), so
  it would feed epoll types to a kqueue platform — wrong until real macOS bindings
  exist.

So the tokio integration is a **Linux‑only** path; the crate's CI also builds
Windows, which would need a second, entirely different design.

## 4. The benefit

- **The motivating factor — default thread footprint.** msquic's worker pool is
  **global (one per process)** and sized to `CxPlatProcCount()` — **one worker
  thread per logical processor**, created on library init and shared across all
  registrations/connections (`src/platform/platform_worker.c:271,293`;
  `src/core/library.c:719-720`), regardless of how many connections exist. On a
  64/128‑core host that is 64/128 threads for even a handful of streams. They are
  soft‑affinitized (ideal‑proc, normal priority — not hard‑pinned/elevated by
  default, `platform_worker.c:298-307`) and **block on `io_uring_wait_cqe` /
  `epoll_wait` when idle** (≈0 CPU, `quic_platform_posix.h:1044-1046`), so idle
  cost is memory/thread‑count, not spin. But under load those up‑to‑P threads run
  datapath + QUIC + app callbacks and **contend with the app's own threads via
  the ordinary OS scheduler**. For a **many‑core host** or a **QUIC‑secondary
  service** (mostly does other work), shrinking that footprint is a legitimate
  goal — the strongest real motivation for wanting execution control.
- **Custom execution's *unique* payoff:** it is the *only* way to eliminate
  msquic's dedicated threads entirely — external execution contexts create **no**
  msquic threads (`platform_worker.c`), so a Tokio‑native service could run msquic
  on its existing Tokio workers and *may* also remove the mandatory
  msquic‑thread → tokio‑task handoff (a context switch / cache‑line bounce per
  event).
- **Honest caveats:** the latency/cache win is **plausible but unquantified**;
  msquic's own scheduler is RSS/NUMA‑aware, so **high‑throughput workloads may
  regress**. Keeping delivery on one worker (to actually remove the handoff)
  requires a pinned/current‑thread design, not ordinary multi‑thread Tokio. And
  crucially, most of the *footprint/contention* goal is reachable far more cheaply
  **without** custom execution — see §7.


## 5. Feasibility blockers

### 5.1 Preview API + no safe Rust wrapper — **decisive**

The custom‑execution surface — `QUIC_EXECUTION_CONFIG`, `ExecutionCreate/Delete/Poll`,
and even `QUIC_GLOBAL_EXECUTION_CONFIG` (processor list / affinity / priority) — is
inside `#ifdef QUIC_API_ENABLE_PREVIEW_FEATURES` in the **public** header
(`src/inc/msquic.h:273-354`; API‑table slots at `:1812-1823`).

**Important nuance (the symbols DO exist at runtime).** That `#ifdef` only gates
the *application's* compile‑time view of the declarations. The msquic **library
itself is always compiled with preview enabled** — `src/core/precomp.h:22` does
`#define QUIC_API_ENABLE_PREVIEW_FEATURES 1` — and the API‑table slots are wired
**unconditionally** (`src/core/library.c:1892-1894`, guarded only by
`#ifndef _KERNEL_MODE`). Confirmed on this host: the local
`libmsquic.so.2.5.8` contains `MsQuicExecutionCreate` / `MsQuicExecutionDelete` /
`MsQuicExecutionPoll` (local `t` symbols; msquic only *exports*
`MsQuicOpenVersion` / `MsQuicClose`, reaching everything else via the table). The
committed Rust bindings carry these slots as **unconditional** fields with baked
offset asserts (`src/rs/ffi/linux_bindings.rs:6601-6603`, offset 272), matching
the library layout. So the runtime symbols and API‑table pointers are present and
callable via raw FFI — this is **not** a runtime‑availability blocker.

What remains decisive:

- **No safe Rust wrapper** — `src/rs/lib.rs` exposes none; only raw FFI
  function‑pointer types and `QUIC_API_TABLE` slots exist
  (`src/rs/ffi/linux_bindings.rs:487-499,6601-6603`). Integrating means
  hand‑writing `unsafe` FFI **and** re‑implementing the `Sqe->Completion(cqe)`
  dispatch against `QUIC_SQE`/`QUIC_CQE` (`linux_bindings.rs:235-241`) — an
  **internal** cxplat ABI, not a supported public Rust contract, that differs per
  platform (epoll/IOCP/kqueue) and can change between minor versions with no
  SemVer signal.
- **Preview instability** — the surface is "subject to breaking changes"
  (`docs/PreviewFeatures.md`). It ships enabled in the libraries today, but its
  shape is not SemVer‑protected.

### 5.2 Cross‑provenance / cross‑version consistency

Both link provenances build msquic with preview enabled (`core/precomp.h:22`), so
the execution slots are present and ABI‑consistent in **both** the system
`native-find` library (2.5.8) and the crate‑built `native-src` library (vendored
2.5.1) — there is **no** "shorter table / shifted offsets" divergence. The
residual version risk is different: the crate's Rust bindings are generated from
**2.5.1**, so any *newer* execution/event semantics introduced in the 2.5.8
runtime (e.g. added event types) would not be decoded by the 2.5.1 `StreamEvent`
mapping, which panics on unknown events (`src/rs/types.rs:371-379`). That is a
bindings‑version concern for any raw‑FFI use of a newer preview surface, not an
API‑table layout mismatch.

## 6. Safety / maintainability cost

- **Callbacks run inline on Tokio workers.** Today callbacks run on msquic's own
  threads; under custom execution they run inside `ExecutionPoll`/completion
  dispatch **on a Tokio worker**. msquic warns any callback delay "will delay the
  protocol" — now it *also* stalls a Tokio worker. `ExecutionPoll` has no
  yield/fairness indication and runs protocol work **plus** app callbacks
  including the receive‑path copy (`stream.rs:834-876`) inline → executor
  starvation / latency coupling under saturation. Mitigation (bounded CQE/work
  batches + explicit yields) is delicate and must be proven under load.
- **Teardown / rundown self‑deadlock.** `RegistrationClose` (from
  `Registration::drop`) **synchronously blocks** until every connection/listener
  rundown drains (`docs/registration-lifecycle.md`). If the thread that drives
  `ExecutionPoll` is the Tokio worker, a `Drop` on that worker blocks the very
  loop that must run to complete rundown → deadlock. The current model is safe
  precisely because msquic's own threads drain independently of the executor.
- **Re‑entrancy & panic blast radius.** Callbacks legitimately call back into
  msquic (`StreamShutdown`, close on `ShutdownComplete`); the crate's
  `catch_unwind` defense (`docs/callback-safety.md`) would now execute inside a
  shared runtime worker, widening the failure radius from "one msquic thread" to
  "a Tokio worker."
- **Parallel‑listener assumption.** `ListenerCtxSender` uses an `AtomicBool`
  *because* msquic invokes listener callbacks in parallel
  (`docs/callback-safety.md`). A single tokio‑driven context **serializes** what
  msquic assumes is parallel (a behavior change); multiple contexts re‑introduce
  the multi‑thread complexity the integration was meant to avoid.
- **Runtime‑agnostic regression.** Driving execution from `tokio::AsyncFd`
  **hard‑couples the library to tokio**, discarding the documented
  executor‑agnostic invariant (`registration-lifecycle.md`) and locking out
  `async-std` / `smol` / custom‑executor consumers.

## 7. The thread‑footprint goal is reachable **far more cheaply** than custom execution

If the real motivation is the default per‑core thread footprint / contention
(§4) rather than "share one event loop with the app," there is a much smaller
lever than a custom event loop:

- **`QUIC_PARAM_GLOBAL_EXECUTION_CONFIG`** — a single global `SetParam` with a POD
  struct (`QUIC_GLOBAL_EXECUTION_CONFIG`: `ProcessorCount`, `ProcessorList`,
  affinity/priority `Flags`, `PollingIdleTimeoutUs`) set **before** library init.
  `ProcessorCount`/`ProcessorList` directly cap and pin the worker pool
  (`src/core/library.c:150-158,1049-1110`; `src/platform/platform_worker.c:267-293`),
  e.g. "2 threads on procs 0–1." msquic still owns those (few) threads — but the
  footprint and cross‑core contention are bounded with **no** event loop, **no**
  unsafe SQE/CQE dispatch, **no** callbacks‑on‑tokio‑worker, **no**
  teardown‑deadlock, **no** runtime‑agnostic regression, and identical behavior on
  every platform. This is the right first tool for the "msquic hogs all cores"
  concern. (It is preview‑gated in the header and has no safe Rust wrapper, so it
  still needs a small raw‑FFI `SetParam` — but that is a tiny surface vs. a
  cross‑platform poll loop.) Note it also gives **cleaner latency isolation** than
  custom execution: bounded *dedicated* threads don't fold msquic's protocol work
  into the app's Tokio workers.
- **`ExecutionProfile`** (LowLatency / MaxThroughput / Scavenger / RealTime) is in
  the **safe** `RegistrationConfig::set_execution_profile`
  (`src/rs/config.rs:37-73`) — **not** preview‑gated — but it tunes *scheduling
  behavior*, not the global datapath thread count, so it does **not** by itself
  shrink the per‑core worker pool.

Only custom execution *eliminates* msquic‑owned threads entirely; the
footprint/contention goal that motivates it is >90% addressed by bounding the pool
via global execution config — a config knob, not a rewrite.


## 8. Conditions to revisit

Reconsider only when **all** hold:

1. There is a concrete requirement to **share a single event loop between msquic
   and the application's own I/O** (the one thing execution profiles cannot do) —
   e.g. a custom / io_uring runtime, or strict single‑loop scheduling.
2. The feature has **left preview** and the safe `msquic` Rust crate ships a
   **SemVer‑stable execution wrapper** (context create/poll/delete + a safe
   completion‑dispatch abstraction) covering **Linux, Windows, and macOS**.
3. A benchmark on a **real target workload** shows removing msquic's worker pool
   (and the thread handoff) is a material win that survives msquic's loss of
   RSS/NUMA‑aware scheduling (§9).
4. **Capping/pinning the pool via `QUIC_GLOBAL_EXECUTION_CONFIG` (§7) has proven
   insufficient** for the need — i.e. the goal genuinely requires *zero* dedicated
   msquic threads / one shared loop, not just *fewer, bounded* threads.

If revisited, it should live in a **separate `msquic-tokio` crate** (see §10), as an
**opt‑in, Linux‑first** execution driver — not inside `msquic-h3` — so the
adapter's runtime‑agnostic default path is preserved and the unsafe/preview/tokio
code is isolated with its own soundness tests.

## 9. Suggested proof‑of‑value benchmark (before any implementation)

- A Linux prototype: msquic epoll fd → `AsyncFd`, driver loop racing
  `readable()` against `sleep(wait_ms)`, bounded per‑iteration CQE/work batches.
- Compare against today's default‑threads path on: p50/p99 request latency,
  context switches, thread migrations, CPU/core, and thread count, at 1 / 32 / 256
  concurrent streams and small‑request vs bulk‑transfer workloads.
- Specifically test **saturation** (to expose executor starvation) and a
  **teardown‑under‑load** case (to expose the rundown‑on‑loop‑thread deadlock).
- If it does not clearly beat the default path — including on high‑throughput,
  where msquic's NUMA/RSS scheduler is strong — stop.

## 10. Where this belongs — crate layering

The execution substrate (*how msquic's I/O runs on threads / an event loop*) is
**orthogonal to HTTP/3**. `msquic-h3`'s sole job is bridging msquic to the
hyperium `h3` traits, and it is deliberately runtime‑agnostic (futures core, tokio
dev‑only — `Cargo.toml:38,69`; `docs/registration-lifecycle.md`). A tokio‑coupled
execution driver therefore **does not belong in `msquic-h3`**: putting it there
would break that invariant and conflate two unrelated layers (protocol adaptation
vs. I/O substrate).

**Correct home:** a dedicated crate depending **only on `msquic`** (not on `h3`
or `msquic-h3`) — e.g. `msquic-tokio` / `msquic-async` / `msquic-runtime` — sitting
**below** h3 as a sibling substrate that `msquic-h3` (or any msquic app)
*consumes*, not contains. Better still long‑term: upstream a safe execution
wrapper into the `msquic` crate and keep `msquic-tokio` a thin runtime adapter on
top. This mirrors the ecosystem's transport/protocol split (`h3` the protocol vs.
`h3-quinn` the transport adapter).

Three caveats keep this from being a free win:

1. **A separate crate contains the blast radius but does not dissolve the
   blockers.** No safe wrapper, preview instability, callbacks running **inline on
   tokio workers** (blocking / re‑entrancy / latency coupling), and Windows‑IOCP /
   macOS‑bindings gaps all travel *with the approach* wherever it lives. The crate
   split is a real win for `msquic-h3`'s purity, but the §5–§6 cost is unchanged.
2. **It introduces a cross‑crate lifecycle contract.** `msquic-h3`'s teardown —
   `Registration` Drop blocks on rundown; connections must drop before the
   registration (`docs/registration-lifecycle.md`) — assumes **msquic owns the
   threads that drive rundown**. If `msquic-tokio` drives `ExecutionPoll`, that
   teardown now depends on the driver loop still running (the §6 deadlock becomes a
   *cross‑crate* invariant), so the boundary needs an explicit ownership/shutdown
   interface co‑designed between the two crates.
3. **The motivating concern needs no new crate.** The default per‑core thread
   footprint (§4) is bounded by `QUIC_GLOBAL_EXECUTION_CONFIG` — a
   **process/library‑level `SetParam`** applied before the first `Registration`
   (`src/core/library.c:1049-1110`), above the registration layer and certainly
   above h3. It belongs wherever the *application* configures the msquic library,
   not in `msquic-h3` and not requiring `msquic-tokio`.

**Conclusion:** `msquic-tokio` is the right *home* **if** true zero‑dedicated‑thread
/ single‑loop integration is ever needed (revisit‑condition #4). For merely
"stop hogging cores," it is overkill — the global‑execution‑config knob at the app
level is the answer.


---

### Appendix — key citations

- Custom‑exec API preview‑gated **in the public header only**: `src/inc/msquic.h:273-354`
  (config/structs/fns), `:1812-1823` (API‑table slots) inside
  `#ifdef QUIC_API_ENABLE_PREVIEW_FEATURES`. But the **library always enables
  preview** (`src/core/precomp.h:22`) and wires the slots unconditionally
  (`src/core/library.c:1892-1894`); local `libmsquic.so.2.5.8` contains
  `MsQuicExecutionCreate/Delete/Poll` (runtime symbols present).
- No safe wrapper; raw FFI only: `src/rs/lib.rs` (none); `src/rs/ffi/linux_bindings.rs:235-241,487-499,6601-6603` (slots unconditional; `ExecutionCreate`/`Delete`/`Poll` offset asserts 272/280/288).
- Platform type divergence: Linux `QUIC_EVENTQ=c_int`, `QUIC_CQE=epoll_event`
  (`linux_bindings.rs:235-237`); Windows IOCP/OVERLAPPED (`win_bindings.rs`);
  macOS reuses Linux bindings (`src/rs/ffi/mod.rs`).
- `ExecutionProfile` is safe/stable: `src/rs/config.rs:37-73`.
- Runtime‑agnostic design: `msquic-h3/Cargo.toml:38,69`; `docs/registration-lifecycle.md`.
- Callback/threading model & hazards: `msquic-h3/src/stream.rs:490-617,684,834-929`;
  `docs/callback-safety.md`; `docs/registration-lifecycle.md`.
- Provenance/CI (2.5.8 vs 2.5.1, Windows): `.github/workflows/build.yaml`.
- msquic threading/custom‑exec model: msquic `docs/Execution.md`.
