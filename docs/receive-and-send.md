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

`MAX_RECV_BUFFER` is `1 MiB` per stream (`../msquic-h3/src/lib.rs`). Returning
`QUIC_STATUS_PENDING` from a receive callback pauses delivery for *that stream
only*, which is why the budget is per-stream rather than per-connection. The
resulting connection memory bound is approximately
`(MAX_RECV_BUFFER + one in-flight indication) × max concurrent streams`.

### The budget state machine

Per-stream accounting lives in `RecvState` (`buffered`, `pend_outstanding`,
`pending_indication_len` — the full saved length of a paused indication — and a
`peak` high-water mark) behind a single mutex, driven by `RecvBudget`:

- **`admit`** runs on the callback side as one locked transition: increment the
  buffered count, decide whether this indication must be paused, then publish the
  data last. If publishing to the drain-visible queue fails, the increment is
  rolled back. The decision is expressed by the `Admit` enum: `Accepted`, `Pend`
  (over budget — pause this stream), or `PublishFailed` (rollback).
- **`on_drained`** runs on the drain side when h3 consumes data: decrement the
  buffered count and, if a pause is outstanding and the budget has recovered,
  clear the pause and return the full saved length so the stream can be re-armed.

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
`remaining()` length to `SendLen::{Empty, NonEmpty, Oversized}` against
`MAX_ADAPTER_SEND`, the provisional `16 MiB` per-`send_data` ceiling
(`../msquic-h3/src/error.rs`). The split is against this ceiling — not the raw
`u32` protocol maximum — so the adapter never materializes a near-`u32::MAX` owned
copy just to have msquic reject the aggregate; an oversized request is rejected up
front with `OversizedSend`. Because the ceiling is below `u32::MAX`, every accepted
length fits the native `BufferRef`'s `u32`.

There is exactly **one** production copy path: `copy_into_send_buffer`, a single
`copy_to_bytes(remaining)`. It is called from `send_data` (with h3's `WriteBuf`)
and by the send-copy benchmark (with a byte slice), so the benchmark measures the
real copy.

### Exactly-once ownership transfer

Because msquic completes a send asynchronously, the owned `SendBuffer` must
outlive the callback and be freed exactly once. The transfer is centralized in
`submit_owned_send` (`../msquic-h3/src/lib.rs`) — the single place that
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

The `16 MiB` ceiling bounds a *single* `send_data` call, not the sum of
concurrently outstanding sends. The unbounded-aggregate concern (many large sends
in flight at once) is documented in the send-copy benchmark header and is not
enforced by the adapter today; the ceiling is marked provisional for this reason.

## Related

- The terminal reasons these paths record, and how they become h3 errors, are in
  the [error model](./error-model.md).
- The seams (`RecvExec`, `SendExec`) that let tests inject doubles are in
  [testing](./testing.md).
