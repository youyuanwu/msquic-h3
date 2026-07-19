# Receive and send

This document describes the two data paths of the adapter: the per-stream
receive backpressure that bounds how much peer data the adapter buffers, and the
send path that copies each payload into an owned buffer handed to msquic. The
error/terminal side of these paths is in the [error model](./error-model.md); the
panic-safety of the callbacks that drive them is in
[callback safety](./callback-safety.md).

## Receive backpressure

Without backpressure, a fast peer could make the adapter buffer unbounded data
between h3 polls. The adapter enforces a per-stream budget so a stalled reader
pauses only its own stream, and the connection's total receive memory is bounded
by the number of concurrent streams.

The receive path enforces **two** per-stream caps together — a **byte** budget
and a **unit-count** budget — and pauses the stream when *either* is reached:

- `MAX_RECV_BUFFER` is `1 MiB` per stream (`../msquic-h3/src/stream.rs`), the
  default `max_recv_bytes`. It bounds the undrained received **bytes**.
- `DEFAULT_MAX_RECV_UNITS` is `16384` per stream, the default `max_recv_units`.
  It bounds the number of undrained buffered **units** (one heap allocation /
  queue node per admitted non-empty indication). The byte budget alone does not
  bound this: because msquic's coalescing of small frames is a defeatable
  arrival-timing optimization (not a contract), a peer that paces tiny (e.g.
  1-byte) frames would otherwise force roughly one allocation per byte — on the
  order of a million buffered units for 1 MiB of accounted payload. The
  unit-count cap closes that allocation-amplification vector; a paced-1-byte peer
  is bounded to ≤ `max_recv_units` buffered units per stream.

Both caps are configurable through [`H3Config`](../msquic-h3/src/config.rs) (see
the [architecture](./architecture.md) module map); the defaults equal the
constants above. Returning `QUIC_STATUS_PENDING` from a receive callback pauses
delivery for *that stream only*, which is why the budget is per-stream rather
than per-connection. The resulting connection memory bound is approximately
`(MAX_RECV_BUFFER + one in-flight indication) × max concurrent streams`, with the
unit count similarly bounded by `max_recv_units × max concurrent streams`.

Frames of at least `F = 128` bytes bind on the **byte** budget first — the byte
total reaches `max_recv_bytes` before the unit count reaches `max_recv_units`, so
the unit cap never becomes the binding limit for realistic frame sizes and there
is no behavior change for frames ≥ F.

### The budget state machine

Per-stream accounting lives in `RecvState` (`buffered` bytes, `units`,
`pend_outstanding`, `pending_indication_len` — the full saved length of a paused
indication — and `peak` / `peak_units` high-water marks) behind a single mutex,
driven by `RecvBudget` (which holds `max_bytes` and `max_units`):

- **`admit`** runs on the callback side as one locked transition: increment the
  buffered byte count, charge one **unit** (only for a non-empty indication that
  will enqueue a `Data` event — an empty indication charges none), decide whether
  this indication must be paused (`buffered >= max_bytes || units >= max_units`),
  then publish the data last. If publishing to the drain-visible queue fails, the
  whole transition — bytes, unit charge, both peak watermarks, and the pend
  fields — is rolled back, so a failed publish never leaks a unit charge. The
  decision is expressed by the `Admit` enum: `Accepted`, `Pend` (over budget —
  pause this stream), or `PublishFailed` (rollback).
- **`on_drained`** runs on the drain side when h3 consumes data: decrement the
  buffered byte count and release exactly one unit (each delivered `Data` event
  consumed exactly one queued unit) and, if a pause is outstanding and **both**
  budgets have recovered (`buffered < max_bytes && units < max_units`), clear the
  pause and return the full saved length so the stream can be re-armed.

Accounting happens *before* the data is made drain-visible, so the budget can
never be undercounted by a publish that races the drain.

### Receive-side transitions

The `StreamEvent::Receive` handler copies the full indication into a `BytesMut`
and then decides its fate:

- An **empty non-FIN** indication produces no event at all — it is not treated as
  end-of-stream.
- An indication that fits the budget is accepted and published as `ReceiveEvent::Data`.
- An **over-budget** indication is still copied and published as data, but the
  callback returns `QUIC_STATUS_PENDING` to defer completion of that indication and
  pause further receive callbacks on the stream. The adapter completes it later —
  re-arming delivery — by calling `receive_complete` once h3 has drained enough
  buffered data.
- A **FIN** still enqueues `ReceiveEvent::Fin` even when the same indication is
  over budget, so a clean end is never lost to backpressure.

Re-arming is done through the `RecvExec` seam (see [testing](./testing.md) for why
the seam exists). When h3 drains data in `poll_data`, `on_drained` returns the
saved length and the executor calls `stream.receive_complete(len)` — with the
*full* saved indication length — which tells msquic the paused bytes were
consumed and delivery may resume.

The receive terminal vocabulary is `ReceiveEvent`: `Data`, `Fin`, `Reset(u64)`,
`Connection(ConnectionTerminal)`, and `Internal`. `Internal` is *stream-local* and
deliberately does **not** freeze the shared connection slot (that distinction is
part of the [error model](./error-model.md)). `publish_recv_terminal` is a
first-writer, and a local `stop_sending` sets a sticky local end-of-stream flag so
subsequent `poll_data` calls return a clean end rather than re-reading the channel.

## Send path

Every send payload is copied once into an adapter-owned buffer whose lifetime is
tied to msquic's asynchronous completion. This section covers the
[owned send buffer](#owned-send-buffer) and the exactly-once ownership transfer.

### Owned send buffer

`SendBuffer` (`../msquic-h3/src/buffer.rs`) owns the payload `Bytes` plus a
self-referential native `BufferRef` that points into it. It carries an
`unsafe impl Send` justified by a compile-time `assert_send` guard, because msquic
completes the send on an arbitrary thread.

A payload is classified before any allocation. `classify_send_len` maps the
`remaining()` length to `SendLen::{Empty, NonEmpty, Oversized}` against the
configured `max_send_bytes` ceiling — `MAX_ADAPTER_SEND` (`16 MiB`,
`../msquic-h3/src/error.rs`) by default, or the value supplied via
[`H3Config`](../msquic-h3/src/config.rs). The split is against this ceiling — not
the raw `u32` protocol maximum — so the adapter never materializes a
near-`u32::MAX` owned copy just to have msquic reject the aggregate; an oversized
request is rejected up front with `OversizedSend`. Because the ceiling is
constrained below `u32::MAX` (config validation rejects `max_send_bytes >=
u32::MAX`), every accepted length fits the native `BufferRef`'s `u32`.

There is exactly **one** production copy path: `copy_into_send_buffer`, a single
`copy_to_bytes(remaining)`. It is called from `send_data` (with h3's `WriteBuf`)
and by the send-copy benchmark (with a byte slice), so the benchmark measures the
real copy.

### Exactly-once ownership transfer

Because msquic completes a send asynchronously, the owned `SendBuffer` must
outlive the callback and be freed exactly once. The transfer is centralized in
`submit_owned_send` (`../msquic-h3/src/stream.rs`) — the single place that
`Box::into_raw`s the buffer, hands the raw pointer to msquic as the send's
`client_context`, and, if the submit fails immediately, reclaims it with
`Box::from_raw`. The `StreamExecutor` (implementing the `SendExec` seam) exposes
`submit_send`, `submit_graceful` (a graceful FIN), and `submit_reset` (an
`ABORT_SEND`).

On the completion side, the `SendComplete` arm reconstructs the `Box<SendBuffer>`
from the `client_context` (a single `Box::from_raw`) before enqueueing
`SendEvent::Complete`, so the buffer is freed exactly once per successful submit.
A **null** `client_context` (which would break that invariant) is treated as an
adapter fault: it publishes `SendTerminal::Internal("SendComplete missing client
context")` and a `TerminalWake` rather than dereferencing the null pointer.

A non-consuming finish guard (`finish_started`) ensures that a `poll_ready` after
a finish has started resolves from state without consuming the shared send channel.

### A note on aggregate memory

The `max_send_bytes` ceiling (default `16 MiB`) bounds a *single* `send_data`
call, not the sum of concurrently outstanding sends. The adapter holds at most
one outstanding owned send copy per stream, so per-connection resident send
memory scales as approximately `concurrent-in-flight-streams × max_send_bytes`.

The adapter does **not** enforce an aggregate send budget, and this is a
constraint of the h3 trait contract rather than an omission. h3 (0.0.8) calls the
synchronous `send_data` — which copies and takes ownership of the payload and
returns a `Result`, not a `Poll` — *before* awaiting the only asynchronous
readiness hook, `poll_ready`. Since the copy has already happened by the time any
async hook runs, the adapter cannot refuse or defer a send as backpressure, so an
aggregate send budget is infeasible at the adapter layer.

The mitigation is the configurable per-send ceiling plus this documented scaling:
to bound per-connection send memory, choose a `max_send_bytes` appropriate to the
workload (via [`H3Config`](../msquic-h3/src/config.rs)) and cap the number of
concurrent streams at the **application** level — the adapter cannot cap stream
concurrency itself. The receive side, by contrast, *is* under the adapter's
control (the receive callback can return PENDING), which is why its byte and
unit-count budgets are enforced as real backpressure.

## Related

- The terminal reasons these paths record, and how they become h3 errors, are in
  the [error model](./error-model.md).
- The seams (`RecvExec`, `SendExec`) that let tests inject doubles are in
  [testing](./testing.md).
