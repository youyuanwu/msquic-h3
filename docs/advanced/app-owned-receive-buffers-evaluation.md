# Evaluation: Integrating msquic App‑Owned Receive Buffers into `msquic-h3`

**Question:** Should `msquic-h3` adopt msquic's app‑owned receive‑buffer feature
(`StreamProvideReceiveBuffers` / `QUIC_RECV_BUF_MODE_APP_OWNED` /
`QUIC_STREAM_OPEN_FLAG_APP_OWNED_BUFFERS`) to reduce receive‑path data copies?

**Recommendation: DO NOT integrate now. Defer.** Revisit only when *all* of the
preconditions in [§7](#7-conditions-to-revisit) are met. The realistic benefit is
**one avoided `memcpy` per receive indication**, which is marginal except at very
high sustained throughput, while the cost is a standing `unsafe` soundness
liability on the hottest path, a rewrite of the just‑merged receive budget, and an
upstream dependency on a **preview** API whose safe Rust wrapper — and whose
on‑demand buffer‑needed event — are **absent from the pinned 2.5.1 bindings**
(the runtime symbol itself is present; see [§4.1](#41-no-safe-binding-and-the-on-demand-buffer-needed-event-is-absent-from-the-pinned-251-bindings--decisive)).

Method: code research over the crate + vendored msquic, plus a two‑perspective
review (performance advocate — `gpt-5.6-sol`; feasibility/safety skeptic —
`claude-opus-4.8`), each grounding claims in `file:line`.

---

## 1. What the feature does

App‑owned mode (`QUIC_RECV_BUF_MODE_APP_OWNED`) has the application supply the
memory buffers msquic writes received stream data into. On a `read`, msquic
returns **views into the app's buffers** instead of into its own internal
chunks. The app must keep each buffer valid until its bytes are fully drained,
then recycle it back via `StreamProvideReceiveBuffers`
(msquic `docs/ReceiveBuffer.md`, "Memory ownership" and "AppOwned").

**Crucial nuance — it does *not* remove the network→memory copy.** msquic's
`write` step copies wire data into the (app‑owned) chunk in **every** mode:
`CxPlatCopyMemory(...)` in `QuicRecvBufferCopyIntoChunks`
(`recv_buffer.c:634-671`; the copy loop is at `:665-671`, and the function's own
comment notes it applies "in single/circular modes … [and] multiple/app‑owned
mode", `:646-648`), which `QuicRecvBufferWrite` invokes unconditionally
(`recv_buffer.c:798`); see also `docs/ReceiveBuffer.md`: "a 'write' … **copies**
the data locally". App‑owned mode only removes the **second, adapter‑level**
copy on `read`.

## 2. Current receive‑copy topology in `msquic-h3`

Per received indication (`StreamEvent::Receive`, `stream.rs:834-905`):

1. msquic copies wire bytes → its internal chunk (**unavoidable**, all modes).
2. Adapter copies chunk view → a fresh `BytesMut::with_capacity(total)` and
   `.freeze()`s to `Bytes` (`stream.rs:852-861`). **← this is the only copy
   app‑owned buffers could remove.**
3. `Bytes` is forwarded zero‑copy via an mpsc channel; `H3RecvStream::poll_data`
   returns it unchanged to h3 (`stream.rs:1519-1520,1567-1583`). h3 already
   consumes/splits `Bytes` zero‑copy.

So the entire prize is **removing copy #2: one `memcpy` of the payload per
indication.**

## 3. Quantified benefit (performance advocate)

Saving = one payload‑sized `memcpy` per indication. At 20–40 GB/s memcpy
bandwidth:

| Receive rate | CPU recovered | Memory‑fabric traffic avoided |
|---|---|---|
| 10 Gbit/s  | ~0.03–0.06 core | ~2.5 GB/s |
| 25 Gbit/s  | ~0.08–0.16 core | ~6.25 GB/s |
| 100 Gbit/s | ~0.31–0.63 core | ~25 GB/s |

- **Matters** for sustained **large bodies**, many concurrent downloads, and
  **≥25 Gbit/s** receivers.
- **Negligible / possibly a regression** for typical small HTTP/3 indications
  (32 B–1 KiB): the copy is nanoseconds, and app‑owned bookkeeping (pool
  management, refcounting, extra FFI calls) can cost more than it saves.
- It removes ~half of the *explicit stream‑payload* copy traffic — **not** half
  of receive CPU. Packet processing, TLS decrypt/auth (in place), reassembly,
  HTTP/3 parsing, and msquic's own `write` copy all remain.

## 4. Feasibility blockers

### 4.1 No safe binding, and the on‑demand buffer‑needed event is absent from the pinned 2.5.1 bindings — **decisive**

`msquic-h3` pins `msquic = "2.5.1-beta"` with `default-features=false`
(`Cargo.toml:34`) and does **not** enable `preview-api`
(features only forward `msquic/find|src`, `Cargo.toml:58-59`). In the vendored
`2.5.1-beta`:

- The safe `StreamEvent` enum has **no** buffer‑needed variant, and its `From`
  impl **panics on unknown event types** (`src/rs/types.rs:255-296,371-379`).
- There is **no safe `Stream::provide_receive_buffers`** — only the raw FFI
  function pointer in the API table (`linux_bindings.rs:6599`). The safe `Stream`
  exposes open/start/shutdown/`receive_complete`/`get_stream_id` (plus
  `set_callback_handler`/`close_inner`) — **none** of which provide buffers
  (`src/rs/lib.rs:1135-1233`).
- The safe `QUIC_STREAM_OPEN_FLAG_APP_OWNED_BUFFERS` constant is gated behind the
  Rust `preview-api` cargo feature (`src/rs/types.rs:491-492`) — though the raw
  flag value could be passed via raw FFI.
- **The on‑demand `QUIC_STREAM_EVENT_RECEIVE_BUFFER_NEEDED` event does not exist
  in 2.5.1‑beta at all** — the C stream‑event enum stops at
  `CANCEL_ON_LOSS = 10` (`src/inc/msquic.h:1524`; grep for the symbol across
  `src/inc` and `src/rs/ffi` returns nothing). The
  `StreamProvideReceiveBuffers.md` doc that motivated this task is from msquic
  **`main`**, a newer release than the vendored one.

**Runtime‑availability nuance (unlike a first read might suggest).** The
`#ifdef QUIC_API_ENABLE_PREVIEW_FEATURES` in the public header only gates the
*application's* compile‑time view. The msquic library is **always** built with
preview enabled (`src/core/precomp.h:22` → `#define QUIC_API_ENABLE_PREVIEW_FEATURES 1`)
and wires `StreamProvideReceiveBuffers` into the API table unconditionally
(`src/core/library.c:1887`); the local `libmsquic.so.2.5.8` contains the
`MsQuicStreamProvideReceiveBuffers` symbol. So the *provide* function is callable
at runtime via raw FFI — it is **not** a missing symbol. The decisive gaps are
the missing **safe** Rust wrappers **and** the missing on‑demand event in the
pinned 2.5.1 bindings.

Consequence: in 2.5.1‑beta, app‑owned mode is **push‑only** — the app must
pre‑provide buffers up to the full advertised flow‑control window
(`docs/ReceiveBuffer.md`), with no callback to pull more on demand.

Integrating therefore requires **either** (a) upstreaming safe wrappers (event +
`provide_receive_buffers` method) to the third‑party `msquic` Rust crate and
enabling `preview-api` — a dependency on someone else's roadmap for a **preview**
API they have deliberately not wrapped — **or** (b) dropping to raw `unsafe` FFI,
abandoning the safe binding the entire crate is built on.

### 4.2 Version‑skew (not symbol availability) across link provenance

An earlier draft claimed release libmsquic is built without preview so the symbol
would be absent — **that is not the case**: the library always enables preview
(`core/precomp.h:22`), so `MsQuicStreamProvideReceiveBuffers` is present in both
the system `native-find` library (2.5.8) and the crate‑built `native-src` library
(vendored 2.5.1), and the API‑table layout is consistent with the committed Rust
bindings. The real cross‑version risk is that the Rust bindings are generated from
**2.5.1**: if the 2.5.8 runtime were driven into app‑owned mode and emitted a
newer event type (e.g. a `RECEIVE_BUFFER_NEEDED` added after 2.5.1), the 2.5.1
`StreamEvent` mapping would hit its unknown‑event panic
(`src/rs/types.rs:371-379`). So the constraint is the **2.5.1 bindings' semantics**,
not symbol availability.


## 5. Safety / maintainability cost

- **`unsafe` zero‑copy recycling.** A provided buffer can't return to msquic
  until **every** `Bytes` derived from it is dropped. h3 owns those `Bytes`
  (`type Buf = Bytes`, `stream.rs:1561`) and may `split`/clone/retain them across
  arbitrary await points. Zero‑copy therefore needs a custom refcounted
  `Bytes` owner (e.g. `Bytes::from_owner`) that recycles a slab on last‑drop —
  `Send`+`Sync` across the msquic worker/tokio boundary and **panic‑safe** inside
  callbacks. This is strictly harder than the crate's existing audited send
  buffer (`buffer.rs`), which is single‑owner with one clean `SendComplete`
  reclaim point. It becomes a permanent soundness‑audit liability on the hottest,
  most fuzzable path. The advocate's own "biggest risk": one premature
  drain/re‑provide → mutable aliasing with h3‑held `Bytes` → silent corruption /
  use‑after‑free.

- **Rewrite of the just‑merged receive budget.** Today backpressure = return
  `QUIC_STATUS_PENDING` to pause one stream, re‑arm via `receive_complete(saved_len)`
  (`RecvBudget::admit`/`on_drained`, `stream.rs:319-425,869-904,1579-1581`;
  `docs/receive-and-send.md`). In app‑owned mode the flow‑control window is
  driven **entirely by how much buffer the app provides** — msquic explicitly
  skips window tuning for app‑owned streams (`stream_recv.c:769-774`). Backpressure
  becomes "stop feeding buffers," incompatible with the PENDING model and its
  ~15 dedicated concurrency tests (`stream.rs:2038-2790`). The freshly landed,
  well‑tested byte+unit budget would be **redesigned, not tweaked.**

- **CI/test burden.** Must stay green under `fmt --check` on 1.90.0 **and**
  1.97.0, `clippy --all-targets -D warnings`, the 4 exact‑path gates, and
  loopback conformance across both provenances — plus new deterministic
  "recycle‑exactly‑once‑after‑last‑drop" receive tests, the hardest kind to make
  reliable.

## 6. Points of agreement between the two reviewers

- Net benefit is **one `memcpy` per indication**; msquic still copies
  wire→chunk in all modes.
- The win is real only at **high sustained throughput / large bodies**; it is
  negligible or negative for small frames.
- The **binding gap is real**: the runtime symbol
  `MsQuicStreamProvideReceiveBuffers` is present, but 2.5.1‑beta has **no**
  on‑demand buffer‑needed event and **no safe provide method**; requires
  `preview-api` + upstream safe wrappers or raw‑FFI work.
- The dominant hazard is **buffer‑lifetime correctness** (UAF/aliasing) given
  h3's arbitrary `Bytes` retention.

## 7. Conditions to revisit

Reconsider only when **all** hold:

1. The feature has **left preview** in a msquic release the crate can pin, and
   the safe `msquic` Rust crate ships a `Stream::provide_receive_buffers` method
   **and** a `RECEIVE_BUFFER_NEEDED` (on‑demand) `StreamEvent` variant.
2. The safe `msquic` Rust bindings decode the on‑demand event and expose the
   provide method on **both** link provenances (`native-find` 2.5.x system package
   **and** `native-src` vendored), so there is no bindings‑version behavior split.
3. A profile of a **real target workload** shows the receive‑path copy is a
   material cost (i.e. large bodies at ≥25 Gbit/s), justifying the `unsafe`
   burden. Prove it first with the benchmark in §8.

If revisited, scope it as an **opt‑in** `H3Config` knob (default off), so the
safe copy path stays the default and the zero‑copy path is isolated behind a
feature/flag with its own soundness tests.

## 8. Suggested proof‑of‑value benchmark (before any implementation)

Extend the existing Criterion harness (`msquic-h3/benches/send_copy.rs`) to first
**measure the prize in isolation**:

- Micro: current `BytesMut` copy vs. pooled‑owner `Bytes::from_owner`
  construction at 32 B / 1 KiB / 64 KiB / 1 MiB.
- Encrypted loopback HTTP/3 transfers: 1 KiB / 64 KiB / 1 MiB / ≥1 GiB total at
  1 / 32 / 256 streams.
- Metrics: receiver goodput, cycles/byte, LLC misses, allocation count, p99
  latency, and (for the app‑owned prototype) pool high‑water mark.
- Include a **delayed consumer** to expose lifetime / flow‑control /
  pool‑exhaustion regressions.

If the isolated copy is not a measurable fraction of receive CPU on the target
workload, stop — the feature cannot pay back its cost.

---

### Appendix — key citations

- Current adapter copy: `msquic-h3/src/stream.rs:852-861`; forward/consume:
  `:1519-1520,1567-1583`.
- Receive budget (PENDING backpressure): `msquic-h3/src/stream.rs:319-425,869-904`;
  `docs/receive-and-send.md`.
- msquic write copies in all modes: vendored `src/core/recv_buffer.c` —
  `QuicRecvBufferCopyIntoChunks:634-671` (copy at `:670`), called unconditionally
  by `QuicRecvBufferWrite:798`; `docs/ReceiveBuffer.md`.
- App‑owned skips window tuning: `src/core/stream_recv.c:769-774`.
- Missing safe binding: `src/rs/types.rs:255-296,371-379,491-492`;
  `src/rs/lib.rs:1135-1233`; raw FFI ptr `src/rs/ffi/linux_bindings.rs:6599`.
- Event absent in 2.5.1‑beta: `src/inc/msquic.h:1524` (enum ends at
  `CANCEL_ON_LOSS = 10`); no `RECEIVE_BUFFER_NEEDED` symbol in `src/inc`/`src/rs/ffi`.
- Provide symbol present at runtime (library always builds preview): `src/core/precomp.h:22`
  (`#define QUIC_API_ENABLE_PREVIEW_FEATURES 1`); `src/core/library.c:1887` (unconditional
  API‑table wiring); `MsQuicStreamProvideReceiveBuffers` present in local `libmsquic.so.2.5.8`.
- Not enabled: `msquic-h3/Cargo.toml:34,58-59`.
- Provenance/CI: `.github/workflows/build.yaml`.
