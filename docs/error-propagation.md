# MsQuic to h3 error propagation

This document reviews how `msquic-h3` 0.0.6 maps errors from `msquic`
2.5.1-beta into the `h3` 0.0.8 QUIC traits. It compares the adapter with
`h3-quinn` 0.0.10 and the internal `s2n-quic-h3` adapter.

The review covers errors after a connection has been handed to `h3`. Errors
from constructing a registration, configuration, listener, or connection are
ordinary `Result<_, msquic::Status>` values and are outside the h3 transport
trait boundary.

## Summary

The MsQuic binding exposes the information needed for useful h3 errors, but the
adapter currently discards most of it:

- connection shutdown is always reported as application close code `0`;
- peer stream resets are reported as clean end-of-stream;
- peer `STOP_SENDING` codes are lost;
- `SendStream::reset` panics instead of sending `RESET_STREAM`;
- several normal cancellation and drop races can panic in an MsQuic callback;
- some connection failures become generic stream errors instead of
  `StreamErrorIncoming::ConnectionErrorIncoming`.

This is primarily a state propagation problem. `ConnCtxSender` and
`StreamSendCtx` send data or completion notifications, but they do not carry a
terminal reason. Closing a channel is then used for every kind of shutdown, so
the polling side cannot distinguish FIN, reset, connection close, or an
internal adapter failure.

## h3 error contract

`h3::quic` defines two incoming error levels:

| h3 variant | Meaning |
| --- | --- |
| `ConnectionErrorIncoming::ApplicationClose { error_code }` | The peer closed the QUIC connection with an HTTP/3 application code. |
| `ConnectionErrorIncoming::Timeout` | The QUIC connection timed out. |
| `ConnectionErrorIncoming::InternalError` | The adapter violated its own contract or failed internally. h3 closes with `H3_INTERNAL_ERROR`. |
| `ConnectionErrorIncoming::Undefined` | A transport or implementation error that h3 cannot classify. |
| `StreamErrorIncoming::ConnectionErrorIncoming` | A stream operation failed because the connection failed. |
| `StreamErrorIncoming::StreamTerminated { error_code }` | The peer reset its send side or sent `STOP_SENDING` for the local send side. |
| `StreamErrorIncoming::Unknown` | An unclassified stream-local failure. h3 handles it like stream termination. |

`Ok(None)` from `RecvStream::poll_data` has a different meaning: the peer
finished its sending side cleanly. It must not represent `RESET_STREAM` or a
connection failure.

## Review findings

### 1. Peer resets become clean EOF (high)

[`stream_callback`](../msquic-h3/src/lib.rs#L473) ignores
`StreamEvent::PeerSendAborted { error_code }`. A later
`StreamEvent::ShutdownComplete` drops the receive sender, and
[`poll_data`](../msquic-h3/src/lib.rs#L693) converts the closed channel to
`Ok(None)`.

The HTTP/3 layer therefore cannot distinguish a graceful FIN from
`RESET_STREAM`. Request streams lose the peer's application code, and critical
streams can take the wrong protocol path before h3 detects that the stream
ended.

Expected result:

```text
PeerSendAborted { error_code }
    -> StreamErrorIncoming::StreamTerminated { error_code }
```

### 2. `SendStream::reset` panics (high)

[`H3SendStream::reset`](../msquic-h3/src/lib.rs#L669) is part of the h3 QUIC
trait contract, but it calls `panic!`. h3 uses this operation to abort request
and control streams. A protocol-level stream cancellation can consequently
unwind the executor, and potentially cross an FFI callback boundary depending
on the call path.

It should call:

```rust
let _ = self
  .stream
  .shutdown(StreamShutdownFlags::ABORT_SEND, reset_code);
```

As in the Quinn and s2n adapters, the trait returns no result, so an immediate
local failure can only be ignored or recorded for the next stream poll.

### 3. Every connection close is reported as `H3_NO_ERROR` (high)

[`connection_callback`](../msquic-h3/src/lib.rs#L110) ignores both
`ShutdownInitiatedByPeer { error_code }` and
`ShutdownInitiatedByTransport { status, error_code }`. On
`ShutdownComplete`, it only closes the incoming-stream channels.

Both [`poll_accept_recv`](../msquic-h3/src/lib.rs#L252) and
[`poll_accept_bidi`](../msquic-h3/src/lib.rs#L268) turn that channel closure into:

```rust
ConnectionErrorIncoming::ApplicationClose { error_code: 0 }
```

This changes transport failures, timeouts, local shutdown, and non-zero peer
HTTP/3 closes into a successful peer application close. The code is not merely
missing diagnostic detail: h3 makes control-flow decisions from these variants.

### 4. Peer `STOP_SENDING` is discarded (high)

`StreamEvent::PeerReceiveAborted { error_code }` means the peer no longer
accepts data on the local send side. The callback ignores it. An in-flight send
may later complete as cancelled, but [`poll_ready`](../msquic-h3/src/lib.rs#L590)
can then report only `Unknown(QUIC_STATUS_ABORTED)`. If no send is in progress,
`poll_ready` returns `Ok(())` without observing any terminal state.

Expected result:

```text
PeerReceiveAborted { error_code }
    -> StreamErrorIncoming::StreamTerminated { error_code }
```

The error must be visible from `poll_ready` and `poll_finish`, including when no
send is currently in flight. Note that today `poll_ready` returns `Ok(())`
immediately when `!send_inprogress` and never inspects the send channel in that
branch, so a `PeerReceiveAborted` that arrives while the send side is idle is
invisible. The sticky terminal state must therefore be checked *before* that
early return (see structural change 5).

### 5. Normal cancellation and split-stream drops can panic (high)

Callbacks use `unwrap` or `expect` when a one-shot receiver or stream half may
legitimately have been dropped. Examples include:

- connection establishment is cancelled before `Connected`;
- an h3 receive half is dropped before another receive callback;
- an h3 send half is dropped before `SendComplete`;
- connection shutdown cancels a pending stream start;
- a finish waiter is dropped before `SendShutdownComplete`;
- the listener accept receiver or shutdown waiter is dropped before
  [`listener_callback`](../msquic-h3/src/listener.rs) delivers.

An FFI callback must not panic for these lifecycle races. This audit covers all
three callback surfaces — connection, stream, **and listener**. The listener
path is currently unsafe in the same way: `listener.rs` uses
`unbounded_send(...).expect("cannot send")` for new connections and end-of-accept,
`tx.send(()).expect("cannot send")` for the shutdown waiter, and
`ctx.shutdown.lock().unwrap()` (panics on a poisoned mutex). All of these must
become fallible, non-panicking sends and poison-recovering locks
(`PoisonError::into_inner`), because the design already relies on the binding's
close model for `Listener` as well. Ordinary sender/receiver loss must never
unwind through FFI, and non-callback waiters (`ConnectionShutdownWaiter::wait`,
listener frontend waiters) must likewise not turn dropped-sender into a panic.
Sending to a closed channel should be handled explicitly. There is an additional
ownership constraint for `SendComplete`: the submitted allocation must be
reclaimed even if its receiver is gone.

Do not retain the current generic erasure. It would require at least
`B: Send + 'static`, and [`H3Buff::new`](../msquic-h3/src/buffer.rs) assumes
that advancing an arbitrary `Buf` does not invalidate chunks returned before
the advance. Neither property is part of h3's `B: Buf` contract.

Materialize each `WriteBuf<B>` synchronously into owned `Bytes` before calling
MsQuic. `bytes::Buf::copy_to_bytes(data.remaining())` consumes the complete wire
representation while `B` is still on the caller's thread. The callback-owned
allocation is concrete:

```rust
struct SendBuffer {
  buffers: [BufferRef; 1],
  bytes: Bytes,
}
```

A single send request is capped at `u32::MAX` bytes by MsQuic itself:
`MsQuicStreamSend` sums every `QUIC_BUFFER.Length` into a `u64 TotalLength` and
returns `QUIC_STATUS_INVALID_PARAMETER` when that sum exceeds `UINT32_MAX`,
before the request is queued. Partitioning a larger payload across multiple
`BufferRef` entries therefore does not help — the whole request is rejected — so
the design validates the complete length up front and keeps a single buffer:

```text
remaining == 0              -> successful no-op (no allocation, no native send,
                               send_inprogress stays false)
remaining > u32::MAX        -> Err(StreamErrorIncoming::Unknown(OversizedSend))
                               before any allocation or native submission;
                               finish/terminal state unchanged
0 < remaining <= u32::MAX   -> materialize into owned Bytes, construct one
                               BufferRef, submit one native send
```

`OversizedSend` is a small adapter-owned `Debug + Display + Error` type. An
oversized send is a stream-operation limitation, not a connection failure or a
peer termination, so `StreamErrorIncoming::Unknown` is the closest h3 class.

Construct `bytes` first, then construct the `BufferRef` from its slice and put
both in the box. Moving `Bytes` does not move its referenced byte storage, so
the pointer remains stable. `SendBuffer` is therefore self-referential:
`buffers[0]` points into the heap storage owned by `bytes`, not into the struct,
and stays valid for as long as the box lives. This invariant is load-bearing and
warrants an explicit safety comment at the construction site. Put construction
behind one function taking an already-validated non-zero `u32` length (the
validation from finding 5's precedence table has already rejected empty and
oversized), so `BufferRef::from(&bytes[..])` — whose binding impl casts `usize`
length to `u32` — never truncates. The `bytes` field exists only to own storage;
name it `_bytes` (or read it in a small invariant helper) so it survives the
repository's `-D warnings` policy as an otherwise-unread private field. A test
asserts the `BufferRef` pointer and length still match after the completed
`SendBuffer` is moved into its `Box`. Pass `Box::into_raw(send_buffer)` as
`client_context`. On `SendComplete`, the callback reconstructs `Box<SendBuffer>`
and drops it immediately before enqueueing the completion event. MsQuic
documents that `SendComplete` ends its use of the submitted buffers. On an
immediate `Stream::send` error, `send_data` reconstructs and drops the same box.

This deliberately trades zero-copy sends for a sound implementation of the
existing `B: Buf` API. It adds no `Send` or `'static` bound to h3's generic
buffer type. The cost is one full wire-buffer materialization per non-empty
`send_data`: the call site is `WriteBuf<B>::copy_to_bytes(remaining())`, and
`h3::stream::WriteBuf` does **not** override `copy_to_bytes`, so the default
`bytes::Buf` implementation runs — `BytesMut::with_capacity(len)` then a copy of
every chunk (the frame header array plus the payload), then `freeze()`. This is
an allocation and byte copy even when the inner payload `B` is a single
contiguous `Bytes`; it is not a refcount split, and it doubles peak payload
memory. Because the design accepts requests up to the native `u32::MAX` limit, a
near-4 GiB send attempts a second near-4 GiB allocation before MsQuic is called,
and a Rust allocation failure there may **abort** rather than produce
`OversizedSend`/`StreamErrorIncoming`.

This is a sound choice (it does not justify retaining the unsound generic
`H3Buff` erasure), but its cost is a **release gate**, not optional: add a
benchmark for header-only, small-body, and large contiguous-body sends, document
peak-memory behavior, and make an explicit decision on the size ceiling — accept
the native `u32::MAX` limit as-is, impose a lower adapter limit that rejects with
`OversizedSend` before allocating, or later add internal chunking. The
allocation crossing FFI contains only owned `Bytes` plus pointer metadata, and
the callback knows its concrete destructor. The old generic `H3Buff` can be
deleted after its remaining uses are removed.

### 6. Pending stream open can panic on connection close (medium)

[`poll_open_inner`](../msquic-h3/src/lib.rs#L341) expects the start one-shot to
produce a value. `StreamEvent::ShutdownComplete` can instead drop its sender,
making the one-shot return `Err(Canceled)`. The subsequent `expect("cannot
receive")` panics.

A connection-caused cancellation should be
`StreamErrorIncoming::ConnectionErrorIncoming`. If no connection reason is
available, it should be `Unknown`, never a panic.

`poll_open_inner` checks the shared connection terminal before creating a
native stream and again before polling an existing `OpeningStream`. A published
terminal drops any opening holder and immediately returns
`StreamErrorIncoming::ConnectionErrorIncoming`. If the start channel is
cancelled without a published reason, it returns
`ConnectionErrorIncoming::InternalError` rather than waiting or panicking.

### 7. Send state has two contract hazards (medium)

[`send_data`](../msquic-h3/src/lib.rs#L621) panics when called before the prior
send becomes ready. Quinn and s2n classify this as an adapter/h3 contract
failure using `StreamErrorIncoming::ConnectionErrorIncoming` containing
`ConnectionErrorIncoming::InternalError`; that is safer than unwinding.

[`poll_finish`](../msquic-h3/src/lib.rs#L647) calls MsQuic graceful shutdown on
every poll and treats every `SendShutdownComplete`, including
`graceful: false`, as success. It should record whether shutdown was initiated,
call it once, and resolve from a terminal send event. A non-graceful completion
must use the peer stream error or connection error when available.

### 8. Empty receive data is treated as EOF (medium)

The receive callback drops its sender when an event contains no non-empty
buffers, even without `ReceiveFlags::FIN`. A zero-length notification is not by
itself a graceful stream end. Only FIN or `PeerSendShutdown` should enqueue a
clean EOF marker.

### 9. Connection establishment loses the transport status (medium)

This occurs before the connection reaches h3, but it is part of the public
error path. A transport shutdown during `Connection::connect` is ignored until
`ShutdownComplete` drops the `connected` one-shot. The connect future then
returns a synthetic `QUIC_STATUS_ABORTED`, losing useful statuses such as
handshake failure, timeout, connection refusal, or ALPN negotiation failure.

The connection-establishment one-shot should carry `Result<(), Status>`.
`ShutdownInitiatedByTransport` can send its status directly. A peer application
close has no lossless representation in the existing `Result<_, Status>` API;
the implementation keeps the existing public API and returns
`QUIC_STATUS_ABORTED` in that case, while still storing the exact
`PeerApplication(error_code)` in the shared connection terminal state. A future
public connect error enum can expose that code without changing the h3 mapping.
All one-shot sends are fallible because cancellation of the connect future is
normal.

### 10. Stream ID access can panic after shutdown (medium)

[`get_id`](../msquic-h3/src/lib.rs#L675) queries a native parameter and unwraps
both the MsQuic result and h3 conversion from the infallible `send_id` and
`recv_id` trait methods. A late ID query after shutdown can therefore panic,
and these trait methods have no error channel.

Cache a validated `h3::quic::StreamId` before exposing an `H3Stream`. For local
streams, replace the temporary `H3Stream` in `poll_open_inner` with an
`OpeningStream`; `StartComplete` sends `Result<u64, Status>`, and a successful
poll validates the ID before constructing `H3Stream` with concrete ID fields in
both halves. A local-stream ID that fails h3 validation is an adapter/internal
error surfaced as nested `ConnectionErrorIncoming::InternalError`, not
`Unknown(Status)`.

For accepted streams, ownership of the native handle is the load-bearing detail.
`PEER_STREAM_STARTED` delivers a `StreamRef` (a borrow); the current adapter
takes ownership eagerly with `Stream::from_raw(stream.as_raw())`. **Validate
before taking ownership**, because a native close through Rust *and* a failed
callback return would double-close the same handle: MsQuic's `stream_set.c` runs
`if (QUIC_FAILED(Status)) { QuicStreamClose(Stream); Stream = NULL; }` on the
failure path (before its `HandleClosed` check), so dropping an owning `Stream`
and *also* returning failure asks MsQuic to close a handle the app already
closed — a use-after-free. The two fallible steps are therefore performed
against the **borrowed** handle:

1. `get_stream_id` against the borrowed `StreamRef`/raw handle
   (`Result<u64, Status>`) — do not create an owning `Stream` merely to query.
   If the binding lacks a borrowed query, add one or use the public parameter
   API on the raw handle.
2. `u64::try_into::<h3::quic::StreamId>() -> Result<_, h3::quic::InvalidStreamId>`
   — MsQuic IDs are 62-bit so this is impossible by contract, but still handled.

Ownership rule:

- **Pre-ownership failure** (either step fails before `Stream::from_raw`):
  publish the connection terminal, then return the failure `Status` from the
  callback and take no Rust ownership. MsQuic remains the sole closer and closes
  the rejected stream itself.
- **Success**: only now call `Stream::from_raw`, attach the callback, construct
  `H3Stream` with concrete ID fields in both halves, and enqueue it.
- **Post-ownership delivery failure** (Rust owns the stream but the incoming
  channel send fails): drop/close the stream through Rust and return **success**
  from the callback, so MsQuic's `HandleClosed` branch recognizes the immediate
  application close and does not close again.

Remove the on-demand `get_id` helper. On the pre-ownership failure path the
adapter publishes a connection terminal so the failure is h3-visible rather than
only a native status. Returning a failure status from `connection_callback` does
**not** tear down the connection — `stream_set.c` only closes the individual
rejected stream. The connection-level result comes from the published terminal:
a later `poll_accept_*` observes `ConnectionErrorIncoming::InternalError`, and
h3's connection-error handling closes the QUIC connection with
`H3_INTERNAL_ERROR`. The publication uses the same first-writer rules as every
other connection terminal:

1. Publish the winning h3 connection terminal and wake both acceptors: if
   `ConnectionState` already holds a terminal reason, that winner is preserved;
   otherwise publish `ConnectionTerminal::Internal` — with
   `"accepted stream ID query failed"` for a native `get_stream_id` error, or
   `"accepted stream ID is invalid"` for an h3 `InvalidStreamId`. Do not hold the
   connection-state lock during the enqueue.
2. Return failure from `PEER_STREAM_STARTED` (the preserved native `Status`, or
   `QUIC_STATUS_INTERNAL_ERROR` for an invalid h3 ID) so MsQuic closes the
   rejected stream — the adapter never took Rust ownership, so it never closes.
3. h3 observes `ConnectionTerminal::Internal` via the accept channel and invokes
   connection close with `H3_INTERNAL_ERROR`.

The rejected stream is never handed to h3.

**Queue policy for an internal terminal.** Unlike a normal connection shutdown,
an accepted-stream attachment failure is a *fail-fast* adapter-internal error:
`ConnectionTerminal::Internal` must not be drained behind streams still queued
in the incoming channels, because the adapter has declared its own connection
state invalid and should not keep handing new streams to h3. `poll_accept_recv`
and `poll_accept_bidi` therefore check the sticky connection terminal **before**
dequeuing another stream once an `Internal` terminal is published. Normal
peer/transport shutdown keeps the drain-then-terminal ordering (already queued
streams are delivered first); only the internal-failure class is fail-fast. A
test asserts each policy.

## Reference adapter behavior

The reference adapters preserve the same boundaries:

- `h3-quinn` maps peer application close to `ApplicationClose`, timeout to
  `Timeout`, and other connection failures to `Undefined`.
- Quinn open-stream failures are wrapped in
  `StreamErrorIncoming::ConnectionErrorIncoming`.
- Quinn `ReadError::Reset` and `WriteError::Stopped` become
  `StreamTerminated` with the peer code.
- `s2n-quic-h3` maps `StreamReset` to `StreamTerminated` and a stream's
  `ConnectionError` to `ConnectionErrorIncoming`.
- Both adapters implement `reset` by sending the supplied application code.
- Both adapters report `send_data`-while-busy as `InternalError`, rather than
  panicking.

Sources:

- [h3 0.0.8 QUIC traits](https://docs.rs/h3/0.0.8/h3/quic/index.html)
- [h3-quinn 0.0.10 adapter](https://docs.rs/h3-quinn/0.0.10/src/h3_quinn/lib.rs.html)
- [s2n-quic-h3 adapter](https://github.com/aws/s2n-quic/blob/main/quic/s2n-quic-h3/src/s2n_quic.rs)

## Required mapping

Terminal precedence is scoped rather than global:

- the first connection terminal reason wins for the connection;
- the first receive-side terminal reason wins for each stream;
- the first send-side terminal reason wins for each stream.

Consequently a receive reset does not suppress a distinct peer `STOP_SENDING`
on the send half. Later `ShutdownComplete` events perform cleanup but do not
overwrite an earlier terminal reason in the same scope.

| MsQuic source | h3 result |
| --- | --- |
| `ConnectionEvent::ShutdownInitiatedByPeer { error_code }` | `ApplicationClose { error_code }` |
| Transport status `QUIC_STATUS_CONNECTION_TIMEOUT` or `QUIC_STATUS_CONNECTION_IDLE` | `Timeout` |
| Other `ShutdownInitiatedByTransport { status, error_code }` | `Undefined(Arc::new(MsQuicTransportError { status, error_code }))` |
| Local connection close | `Undefined` with an adapter-owned local-close error; do not invent a peer code |
| Connection channel closes without a recorded reason | `InternalError("connection closed without a terminal reason")` |
| `StreamEvent::PeerSendAborted { error_code }` | Receive side: `StreamTerminated { error_code }` |
| `StreamEvent::PeerReceiveAborted { error_code }` | Send side: `StreamTerminated { error_code }` |
| Stream shutdown caused by connection shutdown | `ConnectionErrorIncoming { connection_error }` |
| Failed `StartComplete` not caused by connection shutdown | `Unknown(Box::new(status))` |
| Cancelled `SendComplete` with recorded peer/connection reason | The recorded `StreamTerminated` or connection error |
| Cancelled `SendComplete` without a reason | `Unknown(QUIC_STATUS_ABORTED)` |
| `Receive` with data and FIN | Deliver data, then `Ok(None)` on the next poll |
| `Receive` with FIN and no data, or `PeerSendShutdown` | `Ok(None)` |
| `Receive` with no data and no FIN | No terminal event |
| `SendShutdownComplete { graceful: true }` | `poll_finish -> Ok(())` |
| `SendShutdownComplete { graceful: false }` | Recorded stream/connection error, otherwise `Unknown` |

The MsQuic transport `error_code` accompanying
`ShutdownInitiatedByTransport` is a QUIC transport code, not an HTTP/3
application code. It should be retained for diagnostics but must not be placed
in `ApplicationClose`.

Conversion helpers implement these exact terminal mappings:

| Adapter terminal | h3 result |
| --- | --- |
| `ConnectionTerminal::PeerApplication(code)` | `ConnectionErrorIncoming::ApplicationClose { error_code: code }` |
| `ConnectionTerminal::Timeout` | `ConnectionErrorIncoming::Timeout` |
| `ConnectionTerminal::Transport { status, error_code }` | `ConnectionErrorIncoming::Undefined(Arc<MsQuicTransportError>)`, retaining both values |
| `ConnectionTerminal::LocalClose` | `ConnectionErrorIncoming::Undefined(Arc<LocalConnectionClose>)` |
| `ConnectionTerminal::Internal(message)` | `ConnectionErrorIncoming::InternalError(message.into())` |
| `ReceiveTerminal::Fin` | `Ok(None)` |
| `ReceiveTerminal::Reset(code)` | `StreamErrorIncoming::StreamTerminated { error_code: code }` |
| `ReceiveTerminal::Connection(reason)` | `StreamErrorIncoming::ConnectionErrorIncoming` using the connection conversion |
| `SendTerminal::Stopped(code)` | `StreamErrorIncoming::StreamTerminated { error_code: code }` |
| `SendTerminal::Connection(reason)` | `StreamErrorIncoming::ConnectionErrorIncoming` using the connection conversion |
| `ReceiveTerminal::Internal` or `SendTerminal::Internal` | `StreamErrorIncoming::ConnectionErrorIncoming` containing `ConnectionErrorIncoming::InternalError` |
| `SendTerminal::LocalReset(code)` | `StreamErrorIncoming::Unknown(Box<LocalStreamReset>)` |
| `SendTerminal::Failed(status)` | `StreamErrorIncoming::Unknown(Box::new(status))` |

`MsQuicTransportError`, `LocalConnectionClose`, and `LocalStreamReset` are
small adapter-owned `Debug + Display + Error` types. The transport error's
display includes both the status and wire transport code.

## Proposed adapter state

Keep callback-facing state in small, adapter-owned enums that are `Clone` where
multiple consumers need the reason. Convert to fresh h3 error values only at
the polling boundary; h3's error enums themselves do not need to be cloneable.

```rust
#[derive(Clone)]
enum ConnectionTerminal {
    PeerApplication(u64),
    Timeout,
    Transport {
        status: Status,
        error_code: u64,
    },
    LocalClose,
    Internal(&'static str),
}

enum ReceiveEvent {
    Data(Bytes),
    Fin,
    Reset(u64),
    Connection(ConnectionTerminal),
}

enum SendEvent {
    Complete {
        cancelled: bool,
    },
  TerminalWake,
    FinishComplete {
        graceful: bool,
    },
}

#[derive(Clone)]
enum ReceiveTerminal {
    Fin,
    Reset(u64),
    Connection(ConnectionTerminal),
    Internal(&'static str),
}

#[derive(Clone)]
enum SendTerminal {
    Stopped(u64),
    Connection(ConnectionTerminal),
    LocalReset(u64),
    Failed(Status),
    Internal(&'static str),
}

struct ConnectionState {
    terminal: std::sync::Mutex<Option<ConnectionTerminal>>,
}

struct StreamState {
    connection: Arc<ConnectionState>,
    send_terminal: std::sync::Mutex<Option<SendTerminal>>,
}

enum IncomingStream<T> {
    Stream(T),
    Terminal(ConnectionTerminal),
}
```

`LocalReset` is deliberately not converted to `StreamTerminated`, which means
termination by the peer. h3 should not poll a send stream after calling
`reset`; if it does, return `Unknown` with an adapter-owned local-reset error.
Recording it still prevents a later non-graceful completion from being reported
as an unrelated cancellation.

### Connection terminal publication

Store `Option<ConnectionTerminal>` behind a mutex in an `Arc` owned by
`ConnHandle`. The connection callback, every locally opened stream, and every
accepted stream receive a clone. The first writer sets the value; subsequent
writes leave it unchanged.

Create the shared state before `msquic::Connection::open` or before installing
the callback on an accepted connection. The callback closure and `ConnHandle`
each receive an `Arc` clone; this is not a cycle because the state does not own
the native connection. `ConnCtxSender` also holds the clone, so
`PeerStreamStarted` calls `H3Stream::attach(stream, state.clone())`.
`H3Stream::open_and_start` accepts `&ConnHandle`, rather than only
`&msquic::Connection`, and copies the same state into locally opened streams.
Each stream creates one `Arc<StreamState>` shared by its callback and send
frontend. Receive terminal ordering remains in the receive event channel;
send-terminal publication uses `StreamState::send_terminal`.

Returned `H3SendStream` and `H3RecvStream` values also contain the cached
`StreamId`. Only `OpeningStream`, which is private to `StreamOpener`, may exist
without one. Thus `send_id` and `recv_id` are plain field reads and remain
infallible after shutdown.

All mutex access uses a helper that recovers poisoned state with
`PoisonError::into_inner`; an FFI callback must not panic merely because another
task previously panicked while holding adapter state. Publication helpers insert
the candidate only if the slot is empty, clone and return the stored winner,
then release the lock before sending wakeups or acquiring any other lock. No
code holds `ConnectionState` and `StreamState` locks at the same time. A
fallback that loses a publication race therefore propagates the actual winner,
not its rejected candidate.

Publish the reason on `ShutdownInitiatedByPeer` or
`ShutdownInitiatedByTransport`, then enqueue it to both incoming-stream
channels. This preserves already queued streams ahead of the terminal event and
wakes both accept methods. `OpenStreams::close` records `LocalClose` before
calling `Connection::shutdown`, because an application-initiated shutdown may
proceed directly to `ShutdownComplete`. Registration-wide shutdown and a bare
`ShutdownComplete` use `LocalClose` if no more specific reason was published.

The unidirectional and bidirectional channels carry
`IncomingStream<H3Stream>`, not nested `Option` values. On `Terminal`, each
accept frontend stores its own sticky copy and returns the converted connection
error. Later polls return that error class without polling again. Channel
closure before a terminal item stores and returns
`Internal("incoming stream channel closed without a terminal reason")`.

Stream callbacks primarily clone the shared reason when
`StreamEvent::ShutdownComplete { connection_shutdown: true, .. }` arrives.
Because connection and stream callbacks may execute concurrently, they also
need this deterministic fallback when the shared slot is still empty:

1. `connection_shutdown_by_app && connection_closed_remotely` becomes
  `PeerApplication(connection_error_code)`.
2. `connection_shutdown_by_app && !connection_closed_remotely` becomes
  `LocalClose`.
3. A timeout `connection_close_status` becomes `Timeout`.
4. Any other connection close becomes `Transport { status:
  connection_close_status, error_code: connection_error_code }`.
5. A `ShutdownComplete` event with `connection_shutdown == false` does not
  publish a connection terminal reason; it represents stream-local shutdown.

This ordering follows MsQuic's implementation: `ConnectionShutdownByApp` is
derived from the connection's `AppClosed` state, which is used for application
close frames from either endpoint, while `ConnectionClosedRemotely` identifies
which endpoint initiated the close. `ConnectionCloseStatus` cannot identify an
application close because arbitrary HTTP/3 application codes may map to a
generic MsQuic status.

The fallback attempts to publish its derived value into the shared slot before
notifying the stream halves. Whichever callback publishes first establishes the
connection-wide result. The transport `error_code` is retained for diagnostics
but is never converted to an HTTP/3 application close code.

### Receive-side transitions

The receive callback enqueues data before a terminal event from the same MsQuic
notification. `Receive` with FIN therefore produces `Data` followed by `Fin`.
`PeerSendShutdown` produces `Fin`; `PeerSendAborted` produces `Reset(code)`.
Empty non-FIN receive notifications produce no event.

`StreamSendCtx` tracks whether a receive terminal event has already been sent.
The first `Fin`, `Reset`, or connection terminal wins and suppresses later
receive events. This prevents the normal MsQuic sequence of `Receive { FIN }`
followed by `PeerSendShutdown` from queueing duplicate FIN events. Data from a
single `Receive { data, FIN }` event is always enqueued before setting and
enqueueing the terminal marker.

`poll_data` drains one event at a time. `Data` returns `Ok(Some(data))`; a
terminal event is stored in `RecvStreamReceiveCtx` and converted to `Ok(None)`
for `Fin`, `StreamTerminated` for `Reset`, or
`ConnectionErrorIncoming` for a connection terminal. Once stored, every later
poll returns the same class of result without polling the channel. A closed
channel without a terminal event becomes `Internal`.

### Send-side transitions

For `SendComplete`, the callback first reconstructs and drops the
`Box<SendBuffer>`, then enqueues `Complete { cancelled }`. It maps
`SendShutdownComplete` to `FinishComplete { graceful }`. On
`PeerReceiveAborted`, it publishes `Stopped(code)` and enqueues `TerminalWake`;
connection-caused `ShutdownComplete` publishes `Connection(reason)` and also
enqueues `TerminalWake`. Failed channel delivery drops only the small event
value and does not panic or affect buffer ownership.

`client_context` must be non-null because every successful adapter send passes
one `Box<SendBuffer>`. The callback checks it with `NonNull::new` before
`Box::from_raw`; a null value publishes `SendTerminal::Internal` and sends a
wakeup event instead of invoking undefined behavior. The design relies on the
MsQuic `StreamSend` contract that a successful submission returns its context
exactly once in `SendComplete`, while a failed submission does not take
ownership and produces no completion.

Send terminal publication is synchronous and shared between callback and
frontend state. `PeerReceiveAborted` stores `Stopped(code)` before enqueueing
its wakeup event; connection shutdown stores `Connection(reason)` before its
wakeup event. `send_data`, `poll_ready`, and `poll_finish` consult this slot
before invoking MsQuic. Unlike receive state, send state has no queued payload
ordering to preserve. MsQuic processes `STOP_SENDING` by delivering
`PeerReceiveAborted` before cancelling send requests, so a following cancelled
`SendComplete` observes the exact `Stopped(code)` reason. A cancelled completion
without any published peer or connection reason remains `Failed(ABORTED)`.

`poll_ready` uses this order:

1. Return the stored `SendTerminal`, if any.
2. Poll queued send events even when no send is in progress. This makes a peer
   `STOP_SENDING` visible before the current early `Ok(())` path.
3. `TerminalWake` reloads and returns the sticky shared terminal. A wake marker
   without a terminal stores and returns `SendTerminal::Internal`.
4. `Complete { cancelled: false }` clears `send_inprogress` and returns
   `Ok(())`; the callback has already reclaimed the buffer.
5. A cancelled completion returns the already recorded peer/connection
   terminal reason. If none exists, store and return
   `Failed(QUIC_STATUS_ABORTED)`; its buffer was also already reclaimed.
6. If no event is queued, return `Pending` while a send is in progress and
   `Ready(Ok(()))` otherwise. Polling the channel first registers the waker in
   either case.

A direct `poll_ready` call after `finish_started` is an adapter/h3 contract
misuse and returns the nested `InternalError`; `poll_finish` processes finish
events directly after that point.

`send_data` first rejects stored terminal state, then returns
`StreamErrorIncoming::ConnectionErrorIncoming` containing
`ConnectionErrorIncoming::InternalError` if `send_inprogress` is already true
or graceful finish has started. No native send occurs in either misuse case.
After successful native submission it sets `send_inprogress = true`. Immediate
native failures use a shared conversion helper: a published connection reason
becomes `ConnectionErrorIncoming`; otherwise the `Status` remains `Unknown`.

`poll_finish` first returns `Ok(())` when `finish_complete` is already set, then
checks a stored terminal error. Successful finish is absorbing: a later
connection shutdown does not retroactively change a completed finish. While a
send is in progress it delegates to `poll_ready`; after readiness it continues
in the same poll. It then submits
`GRACEFUL` exactly once and sets `finish_started`. Subsequent polls only process
events. `FinishComplete { graceful: true }` sets `finish_complete` and returns
`Ok(())`. A non-graceful completion returns an already stored peer/connection
terminal reason, or stores `Failed(QUIC_STATUS_ABORTED)` if no specific reason
exists. A `Complete` event encountered after finish starts is consumed normally;
any impossible event/state combination becomes `SendTerminal::Internal`.
If the native `GRACEFUL` submission fails immediately, publish an already
available connection terminal or `Failed(status)` and return it; do not retry
the shutdown on a later poll.

`reset(code)` emits `SubmitReset(code)` only when no send terminal is stored.
After the native call, it publishes `LocalReset(code)` on success or
`Failed(status)` on failure, each only if a peer or connection terminal did not
win concurrently. A reset may replace a graceful finish that has started but
not completed; after `finish_complete`, reset is a no-op. Because the trait
method is infallible, the stored state is surfaced only if h3 polls the stream
again.

`stop_sending(code)` submits `ABORT_RECEIVE` with the clamped code. Because the
trait method is infallible and terminal — h3 has declared it will no longer
consume the receive side — an immediate `Stream::shutdown` error is treated as
best effort: log it under the `tracing` feature and return. Do not panic and do
not attempt to inject a receive terminal. The clamp to the 62-bit maximum still
applies.

All application codes passed to MsQuic use one helper that clamps `u64` to an
adapter constant `MAX_QUIC_VARINT: u64 = (1 << 62) - 1` without panicking.
(`msquic::u62` is only a `u64` type alias and has no narrowed `MAX`.) Every
outgoing application code is normalized at the FFI boundary regardless of its
Rust wrapper type. This includes `OpenStreams::close`: `h3::error::Code`'s
`From<u64>` performs **no** range check (`Code { code }`) and `value()` returns
the raw `u64`, so a caller can pass `Code::from(u64::MAX)` and violate the same
native precondition guarded for `reset` and `stop_sending`. `close` therefore
applies `clamp_application_code(code.value())` before
`ConnectionShutdownFlags`/`Connection::shutdown`. Boundary tests cover
`(1<<62)-1` (accepted unchanged) and `1<<62` (clamped) for connection close,
`reset`, and `stop_sending`. Incoming MsQuic codes are `u62` and require no
normalization.

### Native stream teardown on drop

**Support policy.** The adapter keeps direct final-owner drop: the native
`msquic::Stream` lives behind an `Arc` shared by both public halves, and
dropping the last owner runs the binding's `Drop`, which calls `StreamClose`.
This is memory-safe and leak-free for send-buffer reclamation (argued below),
and is treated as a *tested compatibility expectation* — asserted by the
outstanding-send teardown test — rather than as a universal guarantee extracted
from MsQuic's documentation. Strict deferred-until-`ShutdownComplete` ownership
is explicitly not adopted.

Dropping the last owner calls `StreamClose` without an explicit prior shutdown.
For reclamation this is well-defined. MsQuic's public stream documentation
(`docs/Streams.md`) states: "If the app closes a stream before it is shutdown,
the stream will be shutdown abortively with an error code of `0`," and further
that an app may "abortively shutdown a stream and immediately close it from the
same thread, without waiting for the `QUIC_STREAM_EVENT_SHUTDOWN_COMPLETE`
event." The v2.5.1 implementation matches this: `QuicStreamClose` on a
started-but-unshutdown stream logs a `CloseWithoutShutdown` warning and performs
an internal `ABORT_SEND | ABORT_RECEIVE | IMMEDIATE` shutdown with
`QUIC_ERROR_NO_ERROR` (code `0`). Because MsQuic runs callbacks inline during
close (it blocks until all work drains) and still dispatches while
`HandleClosed` is set, any outstanding send receives its cancelled
`SendComplete` before the binding drops the callback context. The
`Box<SendBuffer>` is therefore reclaimed exactly once even on drop-triggered
teardown; no explicit shutdown-before-close is required for memory safety or
leak freedom.

MsQuic's public documentation is not fully consistent on close-before-shutdown,
and the design should not paper over that. The API-specific
[`StreamClose.md`](https://github.com/microsoft/msquic/blob/v2.5.1/docs/api/StreamClose.md)
(present upstream, though only top-level `docs/` ships in the crate tarball)
says closing before full shutdown "produces **undefined behavior**," while
`docs/Streams.md` says the stream "will be shutdown abortively with an error
code of `0`." The reconciliation is in `StreamClose.md`'s own explanation: the
unspecified part is strictly the **wire error code** — "clean up ... must be
reflected on the wire with an application specific error code. When the app
closes a stream without first shutting down, MsQuic has to guess which error
code to use (currently uses `0`)." It is not a statement about memory safety or
about whether outstanding sends are completed; the same paragraph confirms
MsQuic does perform the cleanup (with code `0`). So the contradiction is about
protocol-quality (which HTTP/3 code the peer sees), not about `SendBuffer`
reclamation.

The one residual is a quality concern, not a safety gate: a plain drop of a
still-active stream aborts it toward the peer with code `0` rather than a
meaningful HTTP/3 code, and would convert an in-flight graceful finish into an
abort. h3 normally finishes or `reset`s a stream before dropping it, so this
only affects abnormal cancellation. Emitting an explicit abort with a chosen
code before close is a reasonable future enhancement; it is intentionally out of
scope for this change.

#### Why the "native stream owner" redesign is rejected

A prior review proposed gating implementation on an adapter-owned native stream
that defers `StreamClose` until `StreamEvent::ShutdownComplete` is observed,
calling close-before-shutdown undefined behavior. As reconciled above, the
"undefined behavior" language in `StreamClose.md` is scoped to the wire error
code, which MsQuic resolves to `0`; it is not a memory-safety hazard. Exactly-
once `Box<SendBuffer>` reclamation does not depend on an inspected internal: it
follows from the abortive cancellation of outstanding sends (each returns its
`SendComplete` context exactly once) plus the Rust binding keeping the callback
context alive until `StreamClose` returns.

That last point is load-bearing and is **not** specific to this adapter or to
the bundled v2.5.1 source. The `msquic` binding's `close_inner` is identical for
every handle type: `get_callback_ctx()` → native `*Close(...)` → `drop(ctx)`
(the `Connection` variant even comments "During handle close the ctx might still
be used"). The binding already assumes, for all of `Connection`, `Listener`, and
`Stream`, that the native close call drains and dispatches every pending
callback inline before it returns. A `libmsquic` that violated that — delivering
an outstanding `SendComplete` asynchronously *after* close returns — would
deliver into a freed callback context and break the binding wholesale, not
merely this adapter. This is therefore treated as a **tested compatibility
expectation** — asserted by the outstanding-send teardown test below against
whatever library is linked — rather than an ironclad guarantee extracted from
every MsQuic distribution's documentation. Adopting strict deferred-until-
`ShutdownComplete` ownership (a connection-owned close queue with cross-callback
lifetime coordination) is rejected: it adds substantial machinery for no
memory-safety or leak benefit over this tested expectation.

A later review then proposed a lighter `NativeStream { Drop -> shutdown(ABORT |
INLINE) }` wrapper instead of a deferred-close queue. That does not work either
and is also rejected: `MsQuicStreamShutdown` accepts `IMMEDIATE` only when
`Flags` equals *exactly* `IMMEDIATE | ABORT_SEND | ABORT_RECEIVE`, so
`IMMEDIATE | INLINE` is `QUIC_STATUS_INVALID_PARAMETER`; and `ABORT | INLINE`
(without `IMMEDIATE`) does not imply `ShutdownComplete`, so the subsequent field
drop still runs `StreamClose` before full shutdown. A synchronous `Drop` hook
cannot wait for the asynchronous `ShutdownComplete` (which `StreamShutdown.md`
notes is delivered "not necessarily inline to the call"), so the wrapper adds no
memory-safety guarantee over MsQuic's own close-time abort — only wire-code
quality, which is the deferred enhancement below.

Two smaller points from those reviews are retained but deferred, not blocking:

1. **Meaningful abandonment code.** MsQuic's own docs recommend an explicit
   abort with a meaningful code before close (`StreamClose.md`: shutdown with
   `ABORT | IMMEDIATE` and an error code, then close after `SHUTDOWN_COMPLETE`).
   Sending an intentional `ABORT_SEND | ABORT_RECEIVE` code on abnormal
   abandonment (instead of the default `0`) is a protocol-quality enhancement,
   tracked separately. If added, it must use a valid flag combination —
   `ABORT` alone, or the documented `ABORT | IMMEDIATE` — never
   `IMMEDIATE | INLINE`, per the flag-validation rule above.
2. **Native library provenance.** This crate uses `default-features = false`
   (and the `find` dev feature can link a system `libmsquic.so.2`), so the
   linked library may not be the bundled v2.5.1 source. This is acceptable
   because the reclamation behavior relied upon is the general MsQuic close
   model already assumed by the binding for every handle type, and it is
   asserted directly by the outstanding-send teardown test below rather than
   taken on faith.

**Supported native configuration.** The tested compatibility expectation is
scoped to the configurations CI actually exercises, so a downstream user linking
a system library is not silently assumed supported:

- Supported = the exact native MsQuic versions and platform builds listed in the
  CI matrix (the version the `msquic` crate binds, plus any others explicitly
  added), each having passed the outstanding-send teardown conformance test.
- Any other library — a different patch/minor `v2.5.x`, a different major, or a
  system `libmsquic.so.2` — is unsupported until it is added to CI and passes
  that test; such users must run the named test against their library before
  deployment. A floating "all v2.5.x" claim is intentionally avoided.
- Bumping the `msquic` crate or the native library version requires rerunning
  the teardown test and re-reviewing `StreamClose` and send-cancellation
  behavior.
- A conformance failure is reported as an unsupported native environment, not as
  an adapter test flake. The exact matrix and test command live in the
  development documentation once the tests exist.
- **Verify the loaded library, not the requested package.** The current CI
  installs mismatched native builds (Linux apt package `2.5.8`, Windows vcpkg
  baseline) while the Rust crate is `2.5.1-beta`, and a package-manager command
  does not prove which shared library the test process actually loads at
  runtime. Make native version/platform an explicit CI matrix dimension, and
  before the conformance tests query MsQuic's runtime version and **fail** if it
  differs from the matrix entry. Keep "compile compatibility" and "conformance
  support" as separate claims.

**Release gate.** Before release, record the exact supported native
versions/platforms, provenance, and the teardown test command in
[`docs/Development.md`](Development.md) — at least OS, architecture, Rust binding
version, native library version, provenance, and command — and link that matrix
from this teardown section. Until that list exists, "supported" has no auditable
configuration and the release is gated (this is a documentation gate, not an
additional state-machine change).

Implement send transitions as a production reducer, not duplicated branches.
There is exactly one authoritative sticky terminal slot,
`StreamState::send_terminal` (the `Arc`-shared, poison-safe mutex the callback
publishes into). The reducer does **not** keep its own terminal copy; instead
the caller reads the shared winner and passes it into every input, and the
reducer publishes local candidates back through a command:

```rust
struct SendState {
    // `send_submitting` / `finish_submitting` / `reset_submitting` are
    // transaction reservations set before the native call so a reentrant or
    // repeated input cannot emit a second submission; each is cleared when its
    // matching `*Submitted` result is fed back. `send_inprogress` is committed
    // only on `SendSubmitted(Ok)`; `finish_started` only on `GracefulSubmitted(Ok)`.
    send_submitting: bool,
    send_inprogress: bool,
    finish_submitting: bool,
    finish_started: bool,
    finish_complete: bool,
    reset_submitting: bool,
}

/// Which frontend method initiated a `PublishTerminal`, so the reducer can
/// choose the correct continuation once the first-writer winner is known.
enum TerminalContinuation {
    SendData,
    PollReady,
    PollFinish,
    Reset,
}

/// Outcome of polling the `mpsc` send-event channel. Distinguishing `Closed`
/// from `Pending` is required: a channel closed without a terminal item is an
/// adapter-internal error, not "no event yet".
enum SendPoll {
    Pending,
    Event(SendEvent),
    Closed,
}

/// `send_data` payload classification, computed after conversion into
/// `WriteBuf<B>` but before any owned bytes are materialized.
enum SendPayload {
    Empty,
    NonEmpty { len: u32 },
    Oversized { len: usize },
}

enum SendInput {
    /// A `send_data` request; `payload` is classified before allocation.
    SendRequested { payload: SendPayload, terminal: Option<SendTerminal> },
    /// Immediate result of the `SubmitSend` native call.
    SendSubmitted { result: Result<(), Status>, terminal: Option<SendTerminal> },
    /// A `poll_ready` request, carrying the send-channel poll outcome.
    PollReady { poll: SendPoll, terminal: Option<SendTerminal> },
    /// A `poll_finish` request, carrying the send-channel poll outcome.
    PollFinish { poll: SendPoll, terminal: Option<SendTerminal> },
    /// A `reset(code)` request.
    Reset { code: u64, terminal: Option<SendTerminal> },
    /// Immediate result of a `SubmitGraceful` native call.
    GracefulSubmitted { result: Result<(), Status>, terminal: Option<SendTerminal> },
    /// Immediate result of a `SubmitReset` native call.
    ResetSubmitted { code: u64, result: Result<(), Status>, terminal: Option<SendTerminal> },
    /// The actual winner returned by the first-writer publish helper, fed back
    /// after a `PublishTerminal` command, tagged with its initiating method.
    TerminalPublished { winner: SendTerminal, continuation: TerminalContinuation },
}

/// One-shot, non-sticky adapter error for `send_data`. Unlike `SendTerminal`,
/// it is never stored in the shared slot; a later `poll_ready` is unaffected.
enum SendOperationError {
    OversizedSend { len: usize },
    Misuse(&'static str),
}

enum SendCommand {
    /// `Poll::Pending`; the caller has already registered the waker.
    Pending,
    /// Infallible no-op (e.g. `reset` after `finish_complete`).
    NoOp,
    /// `send_data` success / no-op (empty payload); no bytes submitted.
    ReturnSent,
    /// Construct the allocation and call `Stream::send`; result -> `SendSubmitted`.
    SubmitSend,
    /// Call graceful/reset shutdown; result -> `GracefulSubmitted`/`ResetSubmitted`.
    SubmitGraceful,
    SubmitReset(u64),
    /// Re-poll the send channel and feed a fresh `PollFinish`. Used after a
    /// successful graceful submission so a synchronously-queued `FinishComplete`
    /// is consumed and, failing that, the waker is registered before `Pending`.
    RepollFinish,
    /// Publish a local terminal candidate via the first-writer helper; the
    /// resolved winner is fed back as `TerminalPublished { winner, continuation }`.
    PublishTerminal { candidate: SendTerminal, continuation: TerminalContinuation },
    /// Return a one-shot, non-sticky `send_data` error (oversized / misuse).
    ReturnImmediateError(SendOperationError),
    ReturnReady,
    ReturnFinished,
    ReturnError(SendTerminal),
}

/// Pure, native-free transition. Mutates only `SendState` and emits one command.
fn transition(state: &mut SendState, input: SendInput) -> SendCommand;
```

`transition` is pure and holds no native handle and no terminal slot, so it is
exhaustively unit testable. All `SendState` mutation happens inside it; the
caller only executes the returned `SendCommand` and feeds results back as
further inputs. This keeps a single terminal owner and an explicit reducer
boundary for observing and publishing it:

- The caller loads the current `StreamState::send_terminal` winner (poison-safe)
  and passes it as the `terminal` field of the input. Terminal precedence is
  **input-specific, not global** — a shared winner must never pre-empt the
  bookkeeping a `*Submitted` result owes its reservation. `transition` is one
  normative `match` in this exact arm order (the tables below derive from it,
  and reducer tests are table-driven so prose, rows, and code cannot diverge):

  1. **Submitted-result bookkeeping first.** `SendSubmitted`,
     `GracefulSubmitted`, and `ResetSubmitted` always validate and clear their
     matching reservation (or commit success) *before* any terminal is
     considered. Only then is a raced shared winner applied to the frontend
     result. This prevents a stale `send_submitting`/`finish_submitting`/
     `reset_submitting` and lets infallible `reset` end in `NoOp` even when a
     terminal raced in. `GracefulSubmitted(Ok)` always commits
     (`finish_started = true`) and emits `RepollFinish`; the subsequent repoll
     reloads the shared winner, so channel order (not this arm) decides between a
     synchronously-queued graceful completion and a terminal wake.
  2. **Absorbing success next.** `finish_complete` makes `PollFinish` return
     `ReturnFinished` before considering later terminals.
  3. **Queued successful finish next.** A
     `PollFinish(Event(FinishComplete { graceful: true }))` while `finish_started`
     sets `finish_complete` and returns `ReturnFinished` **before** method
     terminal handling — it is the *sole* event allowed ahead of method-terminal
     handling. Channel order is the tiebreaker: if the successful finish event
     was queued ahead of a later `TerminalWake`, finish is absorbing and wins; a
     terminal queued first is processed first and wins. This exception applies
     only to graceful finish — never to a terminal wake or a non-graceful finish.
  4. **Method-specific terminal handling.** A current shared winner produces an
     error for `send_data`/`poll_ready`/`poll_finish`, but `NoOp` for `reset`
     (after clearing any reset reservation). The `poll_ready`-after-finish misuse
     `Internal` is **non-sticky** — the `finish_started` state reproduces it on
     every call, so it is returned directly and not published to the shared slot;
     every other `SendTerminal::Internal` in the reducer *is* published through
     the authoritative slot.
  5. **Event rows last.** The channel poll outcome is processed only when no
     earlier group returns.

  In the tables, shorthand like `winner or Failed(status)` means the executor
  runs first-writer publication with the concrete local candidate shown; the
  returned winner (possibly a raced callback terminal) is what surfaces.
- `send_data` is inside the machine and classifies its payload before allocating
  any bytes. `SendRequested { payload, terminal }` applies this precedence, and
  no owned bytes are materialized before the final row: (1) a shared `terminal`
  winner returns that error; (2) `send_inprogress` or a finish/submission
  reservation is h3 misuse (`ReturnImmediateError(Misuse)`); (3) `SendPayload::Empty`
  is a successful no-op (`ReturnSent`, no reservation, no native send);
  (4) `SendPayload::Oversized` returns the non-sticky
  `ReturnImmediateError(OversizedSend { len })`, leaving all state unchanged and
  publishing nothing; (5) `SendPayload::NonEmpty` reserves `send_submitting =
  true` and emits `SubmitSend`. `SendSubmitted(Ok)` clears `send_submitting`,
  sets `send_inprogress = true` exactly once, and returns `ReturnSent`
  (`send_data`'s `Ok(())`) — not `ReturnReady`, which belongs to `poll_ready`;
  `SendSubmitted(Err)` clears `send_submitting`, leaves `send_inprogress` false
  (the caller reclaims the `SendBuffer` first), and **publishes sticky
  `Failed(status)`** unless a peer or connection terminal already won — a failed
  native send means the stream is unusable, so a later `poll_ready` must not
  report ready. A `SendSubmitted` without an outstanding `send_submitting`
  reservation publishes adapter `Internal`. A synchronous `SendComplete` queued
  by the native call is consumed on the next poll and clears `send_inprogress`
  normally.
- Local terminal candidates (`Failed(status)` from immediate graceful/reset
  failure, `LocalReset(code)` on successful reset, `Internal` for a lone
  `TerminalWake`) are emitted as
  `PublishTerminal { candidate, continuation }`. The caller runs the
  first-writer helper — which returns the actual winner, possibly a
  callback-published `Stopped`/`Connection` that raced in — and feeds
  `TerminalPublished { winner, continuation }` back. No native call or channel
  send occurs while the terminal lock is held. The winner, not the rejected
  candidate, is what every later `send_data`/`poll_ready`/`poll_finish` returns.
  The `continuation` tag records which method initiated the publish, because
  `SendState` alone cannot distinguish them; `&mut self` serialization means at
  most one publish loop is in flight, so the tag is unambiguous. Its result:

  | `continuation` | Result after publication |
  | --- | --- |
  | `SendData` | `Err(StreamErrorIncoming)` from `send_data` |
  | `PollReady` | `Poll::Ready(Err(winner))` |
  | `PollFinish` | `Poll::Ready(Err(winner))` |
  | `Reset` | Return from the infallible method; the winner is stored for a later poll, no second native op |

Keeping submission results and the published winner as explicit inputs — rather
than mutating state around the native call — fixes ordering under synchronous
callbacks and immediate failure.

#### Send transition table

Frontend methods are serialized by `&mut self`; callback events are queued
concurrently but observed only as the `PollReady`/`PollFinish` `poll` outcome
(`SendPoll::{Pending, Event(..), Closed}`). Rows are grouped by the precedence
above: submitted-result bookkeeping, then absorbing finish, then method-specific
terminal, then event rows. Every submission command has exactly one matching
`*Submitted` result input, and a reservation flag
(`send_submitting` / `finish_submitting` / `reset_submitting`) blocks a duplicate
submission until that result arrives. In the tables, `winner` means the shared
terminal passed in (possibly a raced callback `Stopped`/`Connection`).

Submitted-result inputs (bookkeeping runs before terminal precedence):

| Input | State mutation | Command |
| --- | --- | --- |
| `SendSubmitted(Ok)` | require `send_submitting`; clear it; `send_inprogress = true` | `ReturnSent`, or `ReturnError(winner)` if a winner raced in |
| `SendSubmitted(Err)` | require `send_submitting`; clear it; stays not in progress; allocation already reclaimed | `PublishTerminal{winner or Failed(status), SendData}` (sticky) |
| `GracefulSubmitted(Ok)` | require `finish_submitting`; clear it; `finish_started=true` | `RepollFinish` (never `Pending` directly) |
| `GracefulSubmitted(Err)` | require `finish_submitting`; clear it (no retry) | `PublishTerminal{winner or Failed(status), PollFinish}` |
| `ResetSubmitted(Ok)` | require `reset_submitting`; clear it | `PublishTerminal{winner or LocalReset(code), Reset}` |
| `ResetSubmitted(Err)` | require `reset_submitting`; clear it | `PublishTerminal{winner or Failed(status), Reset}` |
| any `*Submitted` without its reservation | none | `PublishTerminal{Internal, <continuation>}` (never panic) |

Request and event inputs:

| Input | Relevant state | State mutation | Command |
| --- | --- | --- | --- |
| `SendRequested{terminal:Some(winner)}` | any | none | `ReturnError(winner)` |
| `SendRequested` | `send_inprogress` / `send_submitting` / finishing | none | `ReturnImmediateError(Misuse)` |
| `SendRequested{Empty}` | idle, no winner | none | `ReturnSent` (no-op, no reservation) |
| `SendRequested{Oversized{len}}` | idle, no winner | none | `ReturnImmediateError(OversizedSend{len})` (state unchanged) |
| `SendRequested{NonEmpty}` | idle, no winner | `send_submitting=true` | `SubmitSend` |
| `Reset` | `finish_complete` or terminal stored | none | `NoOp` |
| `Reset` | `send_submitting`/`finish_submitting`/`reset_submitting` set | none | `PublishTerminal{Internal, Reset}` (impossible under serialized callers; never a 2nd native op) |
| `Reset` | otherwise (incl. incomplete finish) | `reset_submitting=true` | `SubmitReset(code)` |
| `PollReady`/`PollFinish` while a submission reservation is set | any | none | `PublishTerminal{Internal, <caller>}` (impossible under serialized callers) |
| `PollReady` / `PollFinish` `Closed` | any, no winner | none | `PublishTerminal{Internal("send channel closed without a terminal"), <caller>}` |
| `PollReady` / `PollFinish` `Closed` | winner present | none | error(winner) via caller continuation |
| `PollReady` after `finish_started` | any | none | `ReturnError(Internal)` (h3 misuse; no native call) |
| `PollReady(Event(Complete{cancelled:false}))` | send pending | `send_inprogress=false` | `ReturnReady` |
| `PollReady(Event(Complete{cancelled:true}))` | send pending | `send_inprogress=false` | `PublishTerminal{winner or Failed(ABORTED), PollReady}` |
| `PollReady(Event(TerminalWake))` | any | none | `PublishTerminal{winner or Internal, PollReady}` |
| `PollReady(Pending)` | send pending | none | `Pending` |
| `PollReady(Pending)` | idle | none | `ReturnReady` |
| `PollFinish(_)` | `finish_complete` | none | `ReturnFinished` (absorbing) |
| `PollFinish(Event(Complete{cancelled:false}))` | send pending, not finishing | `send_inprogress=false`, `finish_submitting=true` | `SubmitGraceful` |
| `PollFinish(Event(Complete{cancelled:true}))` | send pending, not finishing | `send_inprogress=false` | `PublishTerminal{winner or Failed(ABORTED), PollFinish}` |
| `PollFinish(Event(TerminalWake))` | any (not complete) | none | `PublishTerminal{winner or Internal, PollFinish}` |
| `PollFinish(Pending)` | idle, not finishing | `finish_submitting=true` | `SubmitGraceful` |
| `PollFinish(Event(FinishComplete{graceful:true}))` | `finish_started` | `finish_complete=true` | `ReturnFinished` |
| `PollFinish(Event(FinishComplete{graceful:false}))` | `finish_started` | none | `PublishTerminal{winner or Failed(ABORTED), PollFinish}` |
| `PollFinish(Pending)` | `finish_started`, not complete | none | `Pending` |
| any impossible event/state combination | none | none | `PublishTerminal{Internal, <caller>}` (never panic) |

Notes: submitted-result bookkeeping (top table) always runs before the winner is
applied, so a raced terminal cannot leave a reservation stale or force
infallible `reset` down an error path. Successful finish is absorbing \u2014 the
`finish_complete` row precedes any terminal row; additionally a *queued*
`FinishComplete{graceful:true}` (while `finish_started`) is processed before
method-terminal handling, so if that success event was enqueued ahead of a later
`TerminalWake`, finish wins on channel order (graceful finish only; a terminal
wake or non-graceful finish still lets the shared winner win).
`GracefulSubmitted(Ok)` returns `RepollFinish`, never `Pending` directly:
because `futures` mpsc registers the receiver waker only on a `Pending` poll, a
synchronously-queued `FinishComplete` must be drained (or the waker re-armed) by
re-polling in the same `poll_finish` call, otherwise the wakeup is lost.
Successful `send_data` returns `ReturnSent` (`Ok(())`), never `ReturnReady`
(which is `poll_ready` only). `PollFinish` after consuming a
successful `Complete` continues to `SubmitGraceful` in the same poll. An
unexpected or impossible event is adapter `Internal`, never a panic. Unit tests
call the reducer and prove that repeated finish polls
emit `SubmitGraceful` exactly once and reset emits exactly one
`SubmitReset(code)` without a mock native stream abstraction.


Recommended structural changes:

1. Store the connection's first terminal reason in shared state owned by
  `ConnHandle` and available to the connection callback, stream openers, and
  streams.
2. Change incoming-stream channels to carry either a stream or a terminal
   connection reason. Wake both unidirectional and bidirectional acceptors when
   shutdown begins.
3. Replace implicit stream-channel closure with explicit `ReceiveEvent` and
   `SendEvent` terminal items. Channel closure without an item is an internal
   adapter error.
4. Preserve event order: queued receive data is yielded before a following FIN
   or reset event.
5. Make connection, receive, and send terminal states independently sticky.
   Every poll after termination returns the same class of result without
   invoking MsQuic again.
6. Make all callback sends fallible and non-panicking. Ensure a failed
   `SendComplete` delivery cannot affect buffer reclamation because the callback
   drops its concrete `Box<SendBuffer>` first.
7. Add `finish_started` and `finish_complete` state so graceful shutdown is
   submitted once and later polls are side-effect free.
8. Preserve the existing `B: Buf` trait bounds by owning the complete wire
   bytes before crossing FFI.

## Test plan

Most conversion and channel behavior can be tested without a network by
constructing callback events and polling the corresponding receiver.

### Test seams

Some required cases are not naturally producible from a live valid peer/stream
and need explicit, tightly-scoped seams (production ownership must remain
obvious):

- **Accepted-stream ID validation.** A live MsQuic peer stream always has a
  valid 62-bit ID, so the `get_stream_id`-failure and `InvalidStreamId` branches
  cannot occur naturally. Provide a pure `validate_stream_id(u64) -> Result<StreamId, _>`
  helper (unit-tested directly) and a test-only failpoint around the accepted-ID
  query that fails exactly one attachment after the native callback begins. The
  loopback test asserts all three outcomes: the rejected stream is never
  enqueued, both acceptors observe the winning connection terminal, and the peer
  observes `H3_INTERNAL_ERROR` after h3 handles it.
- **Command executor.** The pure reducer proves which `SendCommand` is emitted,
  but not that the executor calls MsQuic exactly once (or not at all). Route
  native `open`/`send`/`shutdown(GRACEFUL)`/`shutdown(ABORT_*)` through one
  narrow executor boundary whose test implementation counts these calls. This
  keeps the reducer native-free and lets "no native call" (terminal / oversized
  / misuse) and "exactly once" (graceful, reset) claims be asserted at the
  reducer-to-executor wiring without a full loopback. A private call table or
  small test-only probe suffices; the native stream itself need not be generic.

Required unit tests:

- peer application close preserves its non-zero code for both accept methods;
- idle and connection timeout map to `Timeout`;
- another transport status maps to `Undefined`, not `ApplicationClose`;
- `PeerSendAborted` makes `poll_data` return `StreamTerminated` with the code;
- `PeerReceiveAborted` makes `poll_ready` and `poll_finish` return
  `StreamTerminated` with the code;
- data followed by FIN yields `Some(data)` and then `None`;
- an empty non-FIN receive event does not yield EOF;
- connection shutdown while stream start is pending returns a connection error
  without panic;
- stream open after a published connection terminal performs no native open;
- dropping either split stream half before callbacks does not panic;
- failed `SendComplete` delivery reclaims the send allocation;
- cancelled `SendComplete` reclaims the allocation and prefers an already
  recorded peer or connection reason;
- `send_data` while busy returns `InternalError` rather than panicking;
- `send_data` after finish starts returns `InternalError` and performs no
  native send;
- receive reset and send stop on the same bidirectional stream remain
  independently observable;
- a stream connection shutdown can derive and publish a terminal reason before
  the connection callback runs;
- accepted and locally opened streams share the same connection terminal slot;
- accepted-stream attachment failure publishes
  `ConnectionTerminal::Internal`, wakes both acceptors, and does not expose the
  rejected stream; an already published peer or transport terminal wins over
  it;
- a native `get_stream_id` failure returns its `Status` from the callback while
  a synthetic invalid h3 `StreamId` publishes the internal terminal and returns
  `QUIC_STATUS_INTERNAL_ERROR`; a local-stream invalid ID surfaces nested
  `InternalError`;
- queued incoming streams drain in order before a following
  `IncomingStream::Terminal`, for both the unidirectional and bidirectional
  channels;
- FIN from both `ReceiveFlags::FIN` and `PeerSendShutdown` is published once;
- a borrowed, non-`Send` `Buf` is copied before `send_data` returns and is not
  retained across FFI;
- a send length validation helper accepts exactly `u32::MAX`, rejects
  `u32::MAX + 1` as `OversizedSend`, and performs that rejection before any
  materialization or native submission (exercise the pure helper with synthetic
  lengths, not real multi-gigabyte allocations);
- an accepted non-empty send constructs exactly one `BufferRef` whose length
  equals the owned byte length;
- empty `send_data` input is a no-op that does not set `send_inprogress` or wait
  for a completion;
- an oversized-send rejection leaves finish and terminal state unchanged;
- `SendRequested` payload precedence: an empty send after a peer/connection
  terminal returns the winner, an empty send after finish started returns
  `Misuse`, oversized-while-busy follows the misuse-before-size order, and only
  `NonEmpty` sets `send_submitting` and emits `SubmitSend`;
- `SendRequested` reserves exactly one submission: a second `SendRequested`
  while `send_submitting` cannot emit another `SubmitSend`, and both
  `SendSubmitted(Ok)` and `(Err)` clear `send_submitting`;
- a `SendSubmitted` (or other `*Submitted`) without its reservation publishes
  `Internal` and cannot mark the stream in progress;
- an immediate `SendSubmitted(Err)` publishes sticky `Failed(status)` (unless a
  peer/connection terminal wins), so a later `poll_ready` does not report ready;
- a method input arriving while a submission reservation is set publishes
  `Internal` and issues no second native command;
- poisoned adapter mutex state cannot panic an MsQuic callback;
- the send reducer emits `SubmitGraceful` once across repeated finish polls and
  emits one `SubmitReset(code)` for reset;
- the send reducer never sets `send_inprogress` on an immediate
  `SendSubmitted { Err }`, and sets it exactly once on `SendSubmitted { Ok }`;
- successful `send_data` (empty or submitted non-empty) returns `Ok(())` via
  `ReturnSent`, and `SendSubmitted(Ok)` never emits `ReturnReady`;
- `GracefulSubmitted(Ok)` emits `RepollFinish`: a synchronously-queued
  `FinishComplete(true)` is consumed in the same `poll_finish` call, and when no
  finish event is queued the repoll registers the waker before returning
  `Pending` (works both after an idle pending channel and after a ready
  `SendComplete`);
- a `FinishComplete{graceful:true}` queued ahead of a later `TerminalWake`
  returns success and stays absorbing, while a terminal queued first returns the
  terminal;
- a callback-published `Stopped(code)` (or connection terminal) wins over a
  concurrent local `Failed(ABORTED)` / `LocalReset` candidate: the reducer
  surfaces the shared-slot winner fed back via `TerminalPublished`, and every
  later `send_data`/`poll_ready`/`poll_finish` returns that same terminal class;
- a lone `TerminalWake` with no shared winner publishes and returns `Internal`;
- a `SendPoll::Closed` with no shared winner publishes and returns `Internal`;
  with an existing winner it returns that winner, and an idle `poll_ready` never
  reports `ReturnReady` from a closed channel;
- a connection terminal racing with `ResetSubmitted` clears `reset_submitting`
  and returns normally (`NoOp`) from infallible `reset`; a peer stop racing with
  `GracefulSubmitted` clears `finish_submitting` and is returned by
  `poll_finish`; a `*Submitted` input arriving without its reservation yields
  `Internal` rather than a panic;
- `poll_finish` observes a cancelled send `Complete` and returns the exact peer
  stop / connection reason when available, otherwise `Failed(ABORTED)`; a lone
  `TerminalWake` through `poll_finish` becomes `Internal`;
- `poll_ready` after `finish_started` returns nested `InternalError` and issues
  no native call;
- stream IDs are cached before exposure and remain available after native
  shutdown;
- immediate graceful-shutdown failure is sticky and is not retried;
- successful finish stays successful after a later connection shutdown;
- out-of-range application codes are clamped to the QUIC 62-bit maximum before
  FFI for connection close, `reset`, and `stop_sending`: `(1<<62)-1` passes
  unchanged and `1<<62` is clamped;
- listener, connection, and stream callbacks do not panic when a receiver or
  waiter is already dropped, and recover from a poisoned adapter mutex;

Required loopback tests:

- peer `RESET_STREAM` and `STOP_SENDING` propagate exact HTTP/3 codes end to
  end;
- peer connection close propagates its HTTP/3 application code;
- idle timeout is visible as `ConnectionErrorIncoming::Timeout`;
- local h3 cancellation submits `ABORT_SEND` with the expected reset code and
  keeps the process alive;
- a synthetic accepted-stream attachment failure rejects the individual native
  stream and causes the h3 connection to close with `H3_INTERNAL_ERROR`;
- dropping all frontend stream owners while a send is still outstanding drives
  the send-allocation count back to zero (exactly-once `SendBuffer` reclamation)
  with no callback panic. This directly guards the close-time inline-drain
  assumption the ownership model relies on, against whichever `libmsquic` is
  linked. The test must be made deterministically outstanding, because with
  MsQuic send buffering the `SendComplete` usually fires inline during
  `Stream::send`. Disabling send buffering is **not** possible through the
  current `msquic` 2.5.1-beta safe API: `Settings::set_SendBufferingEnabled` is
  generated by an enable-only bitflag macro with signature
  `fn set_SendBufferingEnabled(self) -> Self` that always sets the value to `1`
  (there is no `bool` argument, and no safe way to set it false). Before this
  test — and therefore before the direct-drop ownership model can be considered
  gated — pick and document one feasible mechanism:
  1. extend the `msquic` binding with `set_SendBufferingEnabled(bool)`;
  2. add a narrowly scoped, reviewed raw-settings helper that sets `IsSet` and a
     `false` value; or
  3. define another deterministic way to hold a send outstanding (e.g. withhold
     peer flow-control credit) and prove it against every supported native build.
  The test must **first prove buffering was actually disabled** (or that its
  chosen mechanism produced an outstanding send), then increment an allocation
  counter before native submission, assert it is exactly one immediately before
  dropping the final frontend owner, and assert it returns to zero after drop and
  callback completion. Because Rust's test harness has no "inconclusive"
  outcome, make the setup deterministic and self-checking: retry the controlled
  setup a bounded number of times, and if no live outstanding send can be
  produced, **fail** with a setup-specific message (do not pass or `#[ignore]`,
  since the compatibility requirement was not
  exercised). With buffering disabled and flow-control credit withheld, a setup
  failure indicates a real test-environment problem. Run it for every native
  MsQuic configuration declared supported.

## Implementation order

1. **Callback safety first.** Make every callback send fallible and every lock
   poison-recovering across connection, stream, **and** listener paths, and
   remove all `expect`/`unwrap` panics from the callback surfaces. Add the
   receiver-drop and poisoned-lock tests now. This is the first mechanical phase
   so no later commit leaves an FFI-unwind hazard, and every subsequent edit must
   preserve the invariant that no send, `take`, lock, or pointer check can panic.
2. Introduce conversion helpers, scoped terminal enums, outgoing application-code
   clamping (including `OpenStreams::close`), and table-driven reducer tests.
3. Add the shared connection terminal slot and incoming-terminal propagation;
   resolve and test the normal-shutdown-drain versus internal-failure-fail-fast
   queue policy.
4. Split receive data, FIN, reset, and connection failure into explicit events.
5. Introduce `OpeningStream`; validate accepted IDs while the handle is still
   borrowed and take native ownership only on success (avoiding the double-close
   boundary). Add the attachment-failpoint tests.
6. Replace `H3Buff` with the owned `Bytes` `SendBuffer`, add the command-executor
   seam, and test immediate failure plus callback reclamation.
7. Integrate the send reducer, `reset`, and idempotent finish; run reducer,
   executor, and synchronous-callback tests after each slice.
8. Make deterministic outstanding-send setup possible (per the teardown-test
   mechanism decision), then add loopback conformance tests.
9. Add runtime native-version verification, the supported matrix, and send-copy
   performance/peak-memory results to `Development.md`.

This order fixes callback safety and the error vocabulary first, then moves each
callback path onto it while keeping changes reviewable and bisectable.