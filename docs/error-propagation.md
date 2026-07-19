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

`reset_code` must be clamped to the 62-bit varint maximum before this FFI call,
exactly as the detailed design requires for every outgoing application code (see
the `reset(code)` handling and the `clamp_application_code`/`MAX_QUIC_VARINT`
discussion under [Send-side transitions](#send-side-transitions) below). This
snippet is illustrative of the trait shape only; do not implement the unclamped
path directly from it.

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
  [`listener_callback`](../msquic-h3/src/listener.rs#L49) delivers.

An FFI callback must not panic for these lifecycle races. This audit covers all
three callback surfaces — connection, stream, **and listener**. The listener
path is currently unsafe in the same way: `listener.rs` uses
`unbounded_send(...).expect("cannot send")` for new connections and end-of-accept,
`tx.send(()).expect("cannot send")` for the shutdown waiter, and
`ctx.shutdown.lock().unwrap()` (panics on a poisoned mutex). The same class of
frontend panic also appears outside the callback at `listener.rs:144`
(`self.conn.shutdown.lock().unwrap()`) and `listener.rs:310`
(`sht_tx.send(()).unwrap()`), which this general statement is likewise meant to
cover. All of these must
become fallible, non-panicking sends and poison-recovering locks
(`PoisonError::into_inner`), because the design already relies on the binding's
close model for `Listener` as well. Ordinary sender/receiver loss must never
unwind through FFI, and non-callback waiters (`ConnectionShutdownWaiter::wait`,
listener frontend waiters) must likewise not turn dropped-sender into a panic.
Sending to a closed channel should be handled explicitly. There is an additional
ownership constraint for `SendComplete`: the submitted allocation must be
reclaimed even if its receiver is gone.

Do not retain the current generic erasure. It would require at least
`B: Send + 'static`, and [`H3Buff::new`](../msquic-h3/src/buffer.rs#L14) assumes
that advancing an arbitrary `Buf` does not invalidate chunks returned before
the advance. Neither property is part of h3's `B: Buf` contract.

Materialize each `WriteBuf<B>` synchronously into owned `Bytes` before calling
MsQuic. `bytes::Buf::copy_to_bytes(data.remaining())` consumes the complete wire
representation while `B` is still on the caller's thread. The callback-owned
allocation is concrete:

```rust
// `pub` so the crate-private `mod buffer` can expose a public copy entry point
// (see MF-4); the module is never `pub` and its name is never re-exported, so
// `SendBuffer` stays effectively crate-internal and cannot be named by consumers.
pub struct SendBuffer {
  buffers: [BufferRef; 1],
  _bytes: Bytes,
}

// SAFETY: `_bytes` owns immutable, `Send` heap storage; `buffers` holds only
// inert pointer/length metadata into that storage. The box is transferred to
// MsQuic through `client_context` and reconstructed + dropped exactly once
// after `SendComplete` (or reclaimed by the caller on immediate `Stream::send`
// failure). No Rust code accesses the bytes while native code owns the box, so
// dropping it on an MsQuic worker thread is sound.
unsafe impl Send for SendBuffer {}
// `SendBuffer` is transferred, never shared, so `Sync` is neither needed nor
// implemented. A compile-time `assert_impl_all!(SendBuffer: Send)` guards this.
```

`SendBuffer` is `!Send` by default because `BufferRef` is
`#[repr(transparent)]` over `QUIC_BUFFER { Length: u32, Buffer: *mut u8 }` and a
raw `*mut u8` is `!Send`. The eager `Box::into_raw` -> `client_context` ->
`Box::from_raw` transfer launders the box through `*const c_void`, so the
compiler cannot check the cross-thread drop; the `unsafe impl Send` above makes
that transfer explicit and reviewable (the old `H3Buff` path relied on the same
mechanism via `unsafe impl Send for BufPtr`).

A single send request is capped at `u32::MAX` bytes by MsQuic itself:
`MsQuicStreamSend` sums every `QUIC_BUFFER.Length` into a `u64 TotalLength` and
returns `QUIC_STATUS_INVALID_PARAMETER` when that sum exceeds `UINT32_MAX`,
before the request is queued. Relying on that native limit alone is a poor
ceiling for an *owned-copy* design: a near-`u32::MAX` `send_data` would allocate
a second near-4 GiB buffer before MsQuic is called, and a Rust allocation
failure there can abort the process rather than return an error. The adapter
therefore imposes its own finite ceiling `MAX_ADAPTER_SEND` (**provisional**
decision pending the aggregate-memory / async-blocking analysis recorded under
"MF-5 / T2" in the review-resolutions section: `16 * 1024 * 1024`, i.e. 16 MiB —
comfortably above normal HTTP/3 frame sizes, well below any dangerous
allocation), named separately from the native
`u32::MAX` protocol maximum and tunable later. A `send_data` above the ceiling
is rejected with `OversizedSend` **before** any allocation. Partitioning a
larger payload across multiple `BufferRef` entries is intentionally not done —
internal chunking would require the reducer to track multiple native sends per
h3 `send_data` and is deferred as a separate future design. So the design
validates the complete length up front and keeps a single buffer:

```text
remaining == 0                     -> successful no-op (no allocation, no native
                                      send, send_inprogress stays false)
remaining > MAX_ADAPTER_SEND       -> Err(StreamErrorIncoming::Unknown(OversizedSend))
                                      before any allocation or native submission;
                                      finish/terminal state unchanged
0 < remaining <= MAX_ADAPTER_SEND  -> materialize into owned Bytes, construct one
                                      BufferRef, submit one native send
```

`OversizedSend` is a small adapter-owned `Debug + Display + Error + Send + Sync`
type. The `Send + Sync` bounds are required because h3 boxes it into
`StreamErrorIncoming::Unknown(Box<dyn std::error::Error + Send + Sync>)`
(`h3-0.0.8/src/quic.rs:74`); without them it cannot be boxed into that variant.
An oversized send is a stream-operation limitation, not a connection failure or
a peer termination, so `StreamErrorIncoming::Unknown` is the closest h3 class.

Construct `_bytes` first, then construct the `BufferRef` from its slice and put
both in the box. Moving `Bytes` does not move its referenced byte storage, so
the pointer remains stable. `SendBuffer` is therefore self-referential:
`buffers[0]` points into the heap storage owned by `_bytes`, not into the struct,
and stays valid for as long as the box lives. This invariant is load-bearing and
warrants an explicit safety comment at the construction site. Put construction
behind one function taking an already-validated non-zero `u32` length (the
validation from finding 5's precedence table has already rejected empty and
above-ceiling), so `BufferRef::from(&_bytes[..])` — whose binding impl casts
`usize` length to `u32` — never truncates. The `_bytes` field exists only to own
storage; its leading underscore keeps the repository's `-D warnings` policy happy
for an otherwise-unread private field (or read it in a small invariant helper). A
test asserts the `BufferRef` pointer and length still match after the completed
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
memory. The `MAX_ADAPTER_SEND` ceiling (above) bounds that doubling and removes
the near-4 GiB double-allocation / allocation-abort hazard: a request above the
ceiling is rejected with `OversizedSend` before any allocation.

The per-`send_data` copy is a sound choice (it does not justify retaining the
unsound generic `H3Buff` erasure). `MAX_ADAPTER_SEND = 16 MiB` is **provisional**,
not a committed ceiling: it bounds a single operation's doubling but **not**
aggregate memory across concurrent streams, because the cap is per-operation and
allocation is infallible/abort-on-OOM. Ratifying any concrete value is therefore
**gated on multi-stream measurement first**. Before a value is committed, add
benchmarks for header-only, small-body, and large contiguous-body sends up to the
candidate cap **and** multi-stream aggregate retained-bytes / concurrent-latency
measurements (per MF-5/T2), then either ratify the value, lower the cap, or adopt
internal chunking (a separate future design that would make the reducer track
multiple native sends per `send_data`). Until those measurements are in, 16 MiB
is a provisional working value, not a decided ceiling; see MF-5/T2 for the
decision table and the as-built `MAX_ADAPTER_SEND` constant. The allocation crossing FFI
contains only owned `Bytes` plus pointer metadata, and the callback knows its
concrete destructor. The old generic `H3Buff` can be deleted after its remaining
uses are removed.

### 6. Pending stream open can panic on connection close (medium)

[`poll_open_inner`](../msquic-h3/src/lib.rs#L341) expects the start one-shot to
produce a value. `StreamEvent::ShutdownComplete` can instead drop its sender,
making the one-shot return `Err(Canceled)`. The subsequent `expect("cannot
receive")` panics.

A connection-caused cancellation should be
`StreamErrorIncoming::ConnectionErrorIncoming`. If the start channel is
cancelled without any published connection reason, it should map to the nested
`ConnectionErrorIncoming::InternalError` (the same "channel closed without a
recorded reason" classification used at lines 296-297 and by the required
mapping), never `Unknown` and never a panic. (Resolves review C-1: the earlier
"should be `Unknown`" wording is superseded by `InternalError` so the prose,
the mapping table, and the as-built code agree.)

`poll_open_inner` checks the shared connection terminal before creating a
native stream and again before polling an existing `OpeningStream`. A published
terminal drops any opening holder and immediately returns
`StreamErrorIncoming::ConnectionErrorIncoming`. If the start channel is
cancelled without a published reason, it returns
`ConnectionErrorIncoming::InternalError` rather than waiting or panicking.

### 7. Send state has two contract hazards (medium)

[`send_data`](../msquic-h3/src/lib.rs#L622) panics when called before the prior
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

[`get_id`](../msquic-h3/src/lib.rs#L678) queries a native parameter and unwraps
both the MsQuic result and h3 conversion from the infallible `send_id` and
`recv_id` trait methods. A late ID query after shutdown can therefore panic,
and these trait methods have no error channel.

Cache a validated `h3::quic::StreamId` before exposing an `H3Stream`. For local
streams, replace the temporary `H3Stream` in `poll_open_inner` with an
`OpeningStream`; `StartComplete` sends `Result<u64, Status>`, and a successful
poll validates the ID before constructing `H3Stream` with concrete ID fields in
both halves. Note that `StreamEvent::StartComplete` already carries an
`id: u62` field (`u62 = u64`, verified in `msquic-2.5.1-beta/src/rs/types.rs`),
which the current code discards via `StartComplete { status, .. }` at
[`lib.rs:475`](../msquic-h3/src/lib.rs#L475); the `Result<u64, Status>` payload
can source the local-stream ID directly from that field, so no native ID query
is needed for locally-opened streams. A local-stream ID that fails h3
validation is an adapter/internal error surfaced as nested
`ConnectionErrorIncoming::InternalError`, not `Unknown(Status)`.

For accepted streams, ownership of the native handle is the load-bearing detail.
`PEER_STREAM_STARTED` delivers a `StreamRef` (a borrow) with only `stream`/`flags`
and no ID field, so — unlike local streams — an accepted stream genuinely needs
the borrowed `get_stream_id` query. The current adapter takes ownership eagerly
with `Stream::from_raw(stream.as_raw())`. **Validate
before taking ownership**, because a native close through Rust *and* a failed
callback return would double-close the same handle: MsQuic's `stream_set.c` runs
`if (QUIC_FAILED(Status)) { QuicStreamClose(Stream); Stream = NULL; }` on the
failure path (before its `HandleClosed` check), so dropping an owning `Stream`
and *also* returning failure asks MsQuic to close a handle the app already
closed — a use-after-free. The two fallible steps are therefore performed
against the **borrowed** handle:

1. `get_stream_id` against the borrowed `StreamRef`/raw handle
   (`Result<u64, Status>`) — do not create an owning `Stream` merely to query.
   **Committed binding decision (no `msquic` patch required):** the existing
   `msquic` 2.5.1-beta binding already exposes `Stream::get_stream_id(&self) ->
   Result<u64, Status>` (`msquic-2.5.1-beta/src/rs/lib.rs:1230`), and
   `StreamRef` derefs to `Stream` (`define_quic_handle_ref!(Stream, StreamRef)`
   at `lib.rs:1236` produces `impl Deref<Target = Stream> for StreamRef`). The
   `stream: StreamRef` delivered by `ConnectionEvent::PeerStreamStarted`
   (`types.rs:117`) therefore answers the query directly through auto-deref —
   `stream.get_stream_id()` — with **no owning `Stream` created, no raw
   `Api::get_param` call, and no new binding method or dependency patch**. The
   query is `Api::get_param_auto::<u64>(handle, QUIC_PARAM_STREAM_ID)` internally,
   whose failure surfaces as `Err(Status)`. Do not use the raw parameter API and
   do not fork on adding a borrowed query; the deref query is the single chosen
   implementation. See the accepted-stream ID query in `msquic-h3/src/lib.rs`
   for the exact signature and safety contract.
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
small adapter-owned `Debug + Display + Error + Send + Sync` types. The
`Send + Sync` bounds are mandatory: h3 stores these behind
`ConnectionErrorIncoming::Undefined(Arc<dyn std::error::Error + Send + Sync>)`
and `StreamErrorIncoming::Unknown(Box<dyn std::error::Error + Send + Sync>)`
(`h3-0.0.8/src/quic.rs:34,74`), so a type lacking them cannot be `Arc`'d or
boxed into those variants. The transport error's display includes both the
status and wire transport code.

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

**Commit-on-delivery freeze (as built).** The shared connection slot is frozen
(made immutable) **only** when a cause is actually delivered to a caller through
an h3-facing poll — `send_data`/`poll_ready`/`poll_finish` (send frontends),
`poll_open`/stream-start (`StreamOpener`), or `accept` (the accept frontends).
The single freeze primitive is `commit_conn`: it freezes and returns the winner
**only when the slot holds a cause**, and leaves an **empty** slot untouched
(unfrozen) so a later, more-specific cause can still be recorded and delivered.
A non-delivery therefore never freezes: an empty slot yields a synthetic
`InternalError`/`Unknown` without closing the door on a real cause (FR-002 /
FR-003).

The send frontends split the read from the freeze: `resolve_terminal` builds the
reducer input by peeking the slot (connection-precedence, non-freezing), and
`commit_send_winner` performs the actual `commit_conn` freeze at the delivery
point. `reset()` uses `resolve_terminal` for its input **but omits any commit** —
it returns no observable terminal to the caller, so it never freezes the
connection slot and a later specific cause stays deliverable (MF-2). A graceful
finish likewise never reaches a commit, so a successful finish never freezes
(MF-2). The accept path now routes through the same guarded primitive:
`observe_terminal` calls `commit_conn`, freezing only when it actually delivers a
cause and leaving an empty slot refinable on the `InternalError` non-delivery
(Fix 1) — making accept uniform with the send/open/data delivery sites.

The one retained non-`commit_conn` delivery point is `poll_data`'s `Connection`
arm, which still calls `observe_conn_winner` (an unconditional freeze). This is
sound because of the **always-populated-at-delivery** invariant: the stream
callback records the connection reason **before** signalling
`ReceiveEvent::Connection`, so that arm is only ever reached with the slot
already populated and can never freeze an empty slot.

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

**Per-stream receive backpressure (SF-A, as built).** The `Receive` arm copies
the **full** indication into a right-sized `BytesMut` (no data is ever dropped),
then, under a single per-stream `RecvState` lock, commits the byte accounting and
the pend decision and — as the last step of that same locked transition —
publishes the bytes to the drain. Accounting is therefore committed *before* the
bytes become drain-visible, so a concurrent drain can neither underflow the
counter nor miss a pend (a failed hand-off rolls the whole transition back under
the same lock). When a stream's buffered bytes reach `MAX_RECV_BUFFER` (1 MiB
**per stream**) the callback returns `QUIC_STATUS_PENDING` **without** completing
the indication, pausing MsQuic's receive callbacks for **that stream only**. The
drain (`poll_data`) subtracts drained bytes under the same lock and, once a
pended stream falls below the bound, completes the pended indication with its
**full saved length** (never a partial/drained length) via `receive_complete` on
the **same** stream, re-arming its receive callbacks. Peak buffered per stream
stays `≤ MAX_RECV_BUFFER + one in-flight indication` independent of total bytes
sent, so connection memory is bounded by that per-stream bound × the negotiated
max concurrent streams. The native pend/complete handshake is safe without a
native per-stream lock: msquic's `MsQuicStreamReceiveComplete` is lock-free and
tolerates a concurrent/inline completion. This replaces the earlier eager-ack
model that acked all bytes on every indication regardless of reader progress.

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

### Callback panic containment (SF-E)

A defense-in-depth backstop wraps the three adapter callback bodies this crate
controls — connection, stream, and listener — in `guard_callback`, which runs
each body inside `catch_unwind`. Containment does **not** rely on the callback
return status (msquic often ignores it): on a caught panic, stream and connection
recovery explicitly force-close the affected handle through an injectable
`ShutdownSeam`, while listener recovery follows the ownership-aware
close-or-reject rule below. Every class wakes its terminal waiters so a stalled
peer/task is released rather than left hanging. Recovery runs *after*
`catch_unwind` returns (taking `&mut ctx` and the borrowed handle ref as
parameters, so there is no overlapping borrow) and applies a class-appropriate
outcome:

- **stream** → `Stream::shutdown(ABORT)`;
- **connection** → `Connection::shutdown(NONE)` (no connection `ABORT` flag
  exists) — both with `code = H3_INTERNAL_ERROR` (`0x0102`);
- **listener** → ownership-aware recovery (close-or-reject, never both), driven
  by an `ownership_taken` flag reset at the start of each invocation and set right
  after `from_raw`, so the unwind `Drop` that closes an owned connection is never
  double-closed or leaked.

Recovery also sets a per-ctx `poisoned` flag; `guard_callback` then
short-circuits every later event with an event-aware disposition instead of
re-dispatching the body — teardown-completion events (`ShutdownComplete` /
`StopComplete`) return `Ok(())`; all other events return
`Err(H3_INTERNAL_ERROR)`, notably rejecting ownership-bearing listener
`NewConnection` and connection `PeerStreamStarted` events. A real `Err` from the
body (e.g. the Phase 2 receive `PENDING`) flows through unchanged and does **not**
trigger recovery. The scope is deliberately narrow — only the adapter closure
bodies this crate owns; panics unwinding out of the upstream msquic trampoline
are out of scope. This backstop is defense-in-depth, not a substitute for the
primary **no-panic-on-peer-paths** invariant, which still holds.

### Native stream teardown on drop

**Support policy.** The adapter keeps direct final-owner drop: the native
`msquic::Stream` lives behind an `Arc` shared by both public halves, and
dropping the last owner runs the binding's `Drop`, which calls `StreamClose`.
This is memory-safe and leak-free for send-buffer reclamation, argued **from the
v2.5.1 native `QuicStreamClose` source and the binding's uniform
inline-drain-on-close contract** (both below) — *not* from an adapter unit test,
because the public API cannot deterministically hold a real native send
outstanding across a drop-triggered `StreamClose` (see
[As-built implementation](#as-built-implementation)). What the adapter *does*
unit-test is its own exactly-once `Box<SendBuffer>` reclamation on the production
`SendComplete` callback path (the `CountingExec` seam test); the native
inline-drain-on-close behavior that makes the drain safe is a *documented
compatibility expectation* on the linked library, not a guarantee the adapter
asserts with a test it cannot make deterministic. Strict
deferred-until-`ShutdownComplete` ownership is explicitly not adopted.

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
merely this adapter. This is therefore treated as a **documented compatibility
expectation** on whatever library is linked — argued from the v2.5.1
`QuicStreamClose` source above and the binding's uniform `close_inner` contract,
**not** asserted by an adapter unit test: the public API cannot deterministically
hold a native send outstanding across `StreamClose` (per
[As-built implementation](#as-built-implementation)), so no such native teardown
test exists. The release-gate loopback conformance suite still exercises the
accepted-send `SendComplete`/reclaim-once ordering, and the `CountingExec` seam
test proves the adapter's own exactly-once reclamation bookkeeping — but neither
substitutes for the native inline-drain guarantee, which stands on the source and
binding-contract argument rather than an ironclad statement in every MsQuic
distribution's documentation. Adopting strict deferred-until-
`ShutdownComplete` ownership (a connection-owned close queue with cross-callback
lifetime coordination) is rejected: it adds substantial machinery for no
memory-safety or leak benefit over this documented expectation.

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
   model already assumed by the binding for every handle type. It is argued
   from the `QuicStreamClose` inline-`SendComplete`-drain source and the
   binding's uniform `close_inner` contract (per ~793–805/~862–881), **not**
   from an adapter drop-triggered `StreamClose` teardown test — no such test
   exists, because the public API cannot deterministically hold a native send
   outstanding across the close (see
   [As-built implementation](#as-built-implementation)). The available send
   conformance tests — the `CountingExec` seam tests
   (`accepted_send_reclaims_via_callback_exactly_once`,
   `immediate_send_failure_reclaims_without_completion`) and the loopback
   accepted-send `SendComplete`/reclaim-once ordering — exercise the adapter's
   own exactly-once bookkeeping, not the native inline drain.

**Supported native configuration.** The documented compatibility expectation is
scoped to the configurations CI actually exercises, so a downstream user linking
a system library is not silently assumed supported:

- Supported = the exact native MsQuic versions and platform builds listed in the
  CI matrix (the version the `msquic` crate binds, plus any others explicitly
  added), each having passed the available send conformance tests — the
  `CountingExec` seam tests (outstanding-send retain/reclaim via
  `accepted_send_reclaims_via_callback_exactly_once`, immediate-failure reclaim
  via `immediate_send_failure_reclaims_without_completion`, exactly-once
  `SendComplete`) and the loopback accepted-send `SendComplete`/reclaim-once
  ordering suite. There is no drop-triggered `StreamClose` inline-drain teardown
  test to pass, because the public API cannot deterministically hold a native
  send outstanding across the close; the inline-drain guarantee for that path is
  established by native-source review, not by a test (see below).
- Any other library — a different patch/minor `v2.5.x`, a different major, or a
  system `libmsquic.so.2` — is unsupported until it is added to CI, passes the
  available send conformance tests, and has its `QuicStreamClose` inline
  `SendComplete` drain re-confirmed by source review; such users must run the
  conformance tests against their library and perform that review before
  deployment. A floating "all v2.5.x" claim is intentionally avoided.
- Bumping the `msquic` crate or the native library version requires rerunning
  the available send conformance tests and re-reviewing the `QuicStreamClose`
  inline-drain source plus send-cancellation behavior; the inline-drain
  compatibility argument must be re-confirmed by that source review, not by a
  teardown test.
- A conformance failure is reported as an unsupported native environment, not as
  an adapter test flake. The exact matrix and test commands live in the
  development documentation once the tests exist.
- **Verify the loaded library, not the requested package.** The current CI
  installs mismatched native builds (Linux apt package `2.5.8`, Windows vcpkg
  baseline) while the Rust crate is `2.5.1-beta`, and a package-manager command
  does not prove which shared library the test process actually loads at
  runtime. Make native version/platform an explicit CI matrix dimension, and
  before the conformance tests query MsQuic's runtime version and **fail** if it
  differs from the matrix entry. The query is `PARAM_GLOBAL_VERSION`
  (`0x01000004`) against a null global handle, whose value is `[u32; 4]`
  (`QUIC_PARAM_GLOBAL_LIBRARY_VERSION` is `uint32_t[4]`) — `get_param_auto::<u32>`
  allocates only 4 bytes and returns `QUIC_STATUS_BUFFER_TOO_SMALL`, so the query
  type must be `[u32; 4]`. Compare normalized numeric components (and optionally
  `PARAM_GLOBAL_LIBRARY_GIT_HASH` when two builds share a semantic version), not
  a loosely formatted string. Keep this in the conformance preflight, not in
  `Registration::new` (which must still accept ABI-compatible libraries outside
  the tested-support matrix). Keep "compile compatibility" and "conformance
  support" as separate claims.

**Release gate.** Before release, record the exact supported native
versions/platforms, provenance, and the send conformance test commands in
[`docs/Development.md`](Development.md) — at least OS, architecture, Rust binding
version, native library version, provenance, and commands — and link that matrix
from this teardown section. The gate requires the available send conformance
tests (the `CountingExec` seam tests and the loopback reclaim-once ordering
suite) to pass, and, when the native library version is bumped, the
`QuicStreamClose` inline-`SendComplete`-drain behavior to be re-confirmed by
source review; it does **not** require a drop-triggered `StreamClose` teardown
test, which does not exist. Until that list exists, "supported" has no auditable
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

/// `send_data` payload classification, computed from `remaining()` after
/// conversion into `WriteBuf<B>` but before any owned bytes are materialized.
/// The `NonEmpty`/`Oversized` split is against `MAX_ADAPTER_SEND`, not the raw
/// native `u32::MAX`.
enum SendPayload {
    Empty,
    NonEmpty { len: u32 },        // 0 < len <= MAX_ADAPTER_SEND
    Oversized { len: usize },     // len > MAX_ADAPTER_SEND
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
| `PollReady(_)` | terminal winner present (after reservation check) | `send_inprogress=false` | `ReturnError(winner)` (method-terminal step, before event handling) |
| `PollReady` after `finish_started` | any, no winner | none | `ReturnError(Internal)` (h3 misuse; no native call) |
| `PollReady(Event(Complete{cancelled:false}))` | send pending | `send_inprogress=false` | `ReturnReady` |
| `PollReady(Event(Complete{cancelled:true}))` | send pending | `send_inprogress=false` | `PublishTerminal{winner or Failed(ABORTED), PollReady}` |
| `PollReady(Event(TerminalWake))` | any | none | `PublishTerminal{winner or Internal, PollReady}` |
| `PollReady(Pending)` | send pending | none | `Pending` |
| `PollReady(Pending)` | idle | none | `ReturnReady` |
| `PollFinish(_)` | `finish_complete` | none | `ReturnFinished` (absorbing) |
| `PollFinish(_)` | terminal winner present, not `finish_complete`, not a completed graceful finish | `send_inprogress=false` | `ReturnError(winner)` (method-terminal step, before event handling) |
| `PollFinish(Event(Complete{cancelled:false}))` | send pending, not finishing, no winner | `send_inprogress=false`, `finish_submitting=true` | `SubmitGraceful` |
| `PollFinish(Event(Complete{cancelled:true}))` | send pending, not finishing | `send_inprogress=false` | `PublishTerminal{winner or Failed(ABORTED), PollFinish}` |
| `PollFinish(Event(TerminalWake))` | any (not complete) | none | `PublishTerminal{winner or Internal, PollFinish}` |
| `PollFinish(Pending)` | `send_inprogress`, not finishing | none | `Pending` (waiting for the in-flight data send; no graceful submit yet) |
| `PollFinish(Pending)` | idle, not finishing | `finish_submitting=true` | `SubmitGraceful` |
| `PollFinish(Event(FinishComplete{graceful:true}))` | `finish_started` | `finish_complete=true` | `ReturnFinished` |
| `PollFinish(Event(FinishComplete{graceful:false}))` | `finish_started` | none | `PublishTerminal{winner or Failed(ABORTED), PollFinish}` |
| `PollFinish(Pending)` | `finish_started`, not complete | none | `Pending` |
| any impossible event/state combination | none | none | `PublishTerminal{Internal, <caller>}` (never panic) |

Notes: submitted-result bookkeeping (top table) always runs before the winner is
applied, so a raced terminal cannot leave a reservation stale or force
infallible `reset` down an error path. Successful finish is absorbing — the
`finish_complete` row precedes any terminal row; additionally a *queued*
`FinishComplete{graceful:true}` (while `finish_started`) is processed before
method-terminal handling, so if that success event was enqueued ahead of a later
`TerminalWake`, finish wins on channel order (graceful finish only; a terminal
wake or non-graceful finish still lets the shared winner win). **Method-terminal
step:** after the reservation check (both methods) and the two finish exceptions
(`PollFinish` only — absorbing `finish_complete` and a just-completed graceful
finish), a present shared winner is surfaced as `ReturnError(winner)` *before* any
ordinary event handling. This is why a `Pending` poll can never linger past a
terminal and why `PollFinish(Pending)` can never submit graceful shutdown once a
terminal has won.
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

#### Command execution loop

The reducer is pure; a per-method executor loop runs its commands against MsQuic
(through the counting executor seam) and feeds results straight back into
`transition`. The loop must make bounded progress within one poll — in
particular `RepollFinish` performs one fresh channel poll in the **same**
`poll_finish` call, never deferring to a later external invocation (that would
recreate the lost-waker bug). Normative shape for `poll_finish`:

```text
loop {
    let poll = poll_send_channel(cx);            // Pending | Event(e) | Closed
    match transition(&mut state, PollFinish { poll, terminal: load_winner() }) {
        Pending            => return Poll::Pending,        // waker already registered
        ReturnFinished     => return Poll::Ready(Ok(())),
        ReturnError(t)     => return Poll::Ready(Err(convert(t))),
        SubmitGraceful     => feed(GracefulSubmitted { result: exec.graceful(), .. }),
        PublishTerminal{..} => feed(TerminalPublished { winner: publish(..), .. }),
        RepollFinish       => continue,           // re-poll the channel this call
        _                  => unreachable_internal(),
    }
}
```

Only `Pending`, a return command, or an impossible-state `Internal` exits the
loop; native-submission and terminal-publication results are fed back
immediately, and `RepollFinish` iterates. `poll_ready`, `send_data`, and `reset`
use the same immediate-feedback discipline for their own command sets. Test the
real executor with a stubbed channel that queues `FinishComplete` during the
native-call stub (proving same-call consumption) and one that stays pending
(proving the waker is registered before `Pending`).


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
- **Connection-scoped controls (not thread-local / process-global).** MsQuic
  callbacks run on worker threads and a connection may move between workers, so a
  thread-local failpoint set by the test thread would never fire in the callback,
  and a process-global one-shot flag or counter would race parallel tests. Put
  test controls in state already cloned into the callback, behind `#[cfg(test)]`
  or a private test-support feature: the accepted-ID failpoint is an
  atomic action/counter in `ConnectionState` (fail the next or Nth query for
  that connection, consuming itself atomically and identifying exactly which
  stream is rejected), and the reclamation counter is an `Arc<AtomicUsize>` tied
  to the conformance test's connection/stream family and retained by each test
  `SendBuffer`. Tests must run correctly under the normal parallel harness;
  serializing the whole suite is a weaker, explicitly-stated fallback.
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
- a send length validation helper accepts exactly `MAX_ADAPTER_SEND`, rejects
  `MAX_ADAPTER_SEND + 1` as `OversizedSend`, and performs that rejection before
  any materialization or native submission (exercise the pure helper with
  synthetic lengths, no real allocation);
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
- `poll_finish` while a data send is in flight waits (`Pending`) with **no**
  native shutdown call, and a following `Complete{cancelled:false}` then emits
  exactly one `SubmitGraceful` in the same poll (both paths from the same
  starting state);
- terminal-first ordering (a `TerminalWake` observed before the corresponding
  `Complete`) never triggers a second native call or a buffer leak, even though
  `Complete` may remain queued and `send_inprogress` stays set until drop — the
  sticky terminal is returned and the stream is an intentional dead state;
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
- accepted-send reclamation *ordering* over a real loopback connection: an
  accepted send yields exactly one native `SendComplete`, its `Box<SendBuffer>`
  is reclaimed exactly once, and dropping all frontend stream owners after that
  completion causes no callback panic. This is the genuinely available loopback
  send test. It does **not** hold a send observably outstanding at the instant
  the allocation count is read, and it does **not** exercise `StreamClose`'s
  inline `SendComplete` drain: a **real** loopback send cannot be held observably
  outstanding through the public API — send buffering cannot be disabled through
  the safe `msquic 2.5.1-beta` builder (`set_SendBufferingEnabled` is an
  enable-only bitflag — `fn set_SendBufferingEnabled(self) -> Self` always sets
  `1`, `settings.rs:133`), and a *buffered* send is copied into an internal
  buffer and completed with an immediate `SEND_COMPLETE` regardless of peer
  flow-control backpressure (`msquic-2.5.1-beta/src/core/stream_send.c:478-534`,
  `QuicStreamSendBufferRequest`).

The observably-outstanding retain/reclaim transition and the exactly-once
`SendComplete` ownership contract are therefore proven by **seam tests, not
loopback tests** — the deterministic `CountingExec` send seam described in the
[As-built implementation](#as-built-implementation) section:

- outstanding-send retain/reclaim: the seam retains the submitted
  `Box<SendBuffer>` in a test-held `client_context` slot, so the reclamation
  counter reads exactly 1 until the test explicitly feeds a `SendComplete` back
  through the callback path (counter → 0). This requires **no binding extension,
  no `[patch.crates.io]` entry, and no raw `set_param` helper**; the earlier
  "extend the binding with a `set_SendBufferingEnabled(bool)` setter" and
  "test-only raw-settings fallback" requirements are withdrawn in favor of the
  as-built public-API decision. The seam test proves four distinct facts, in
  order: (1) the deterministic blocking condition was established (the seam
  retained the buffer without a `SendComplete`); (2) exactly one adapter
  allocation remained outstanding after the submit call returned; (3) feeding the
  completion caused exactly one callback reclamation; and (4) the count returned
  to zero before the test's explicit completion point. Because Rust's test
  harness has no "inconclusive" outcome, the setup is deterministic and
  self-checking: if the outstanding allocation cannot be read as 1, the test
  **fails** with a setup-specific message (it is never passed or `#[ignore]`d)
  (`accepted_send_reclaims_via_callback_exactly_once`);
- `SendComplete` ownership contract, both directions: an **accepted** send
  returns its context in exactly one completion; a **rejected** send (controlled
  immediate `Stream::send` failure) produces **no** completion. A callback
  counter asserts the exactly-once allocation invariant across supported native
  versions (`immediate_send_failure_reclaims_without_completion`).

The drop-triggered `StreamClose` inline `SendComplete` drain itself has **no**
native or loopback test — the public API cannot hold a send outstanding while
forcing that drop path — so close-time / inline-drain compatibility is **not**
asserted by any test here. It rests on native-source review (`QuicStreamClose`
inline `SendComplete` drain) plus the binding's uniform `close_inner` close
contract (per ~793–805/~862–881 and the
[As-built implementation](#as-built-implementation) section).

## Society-of-thought review resolutions (round 13)

This section is the stable, authoritative record of how the round-13
society-of-thought review (7 Must-Fix, 10 Should-Fix, 5 Trade-offs, 9 Consider)
is resolved. It is the authoritative design of record; the pre-implementation
scaffold that formerly followed it has been removed (see the
[As-built implementation](#as-built-implementation) section). Trade-offs are decided with the fixed priority hierarchy
**Correctness > Security > Reliability > Performance > Maintainability >
Developer Experience**. Code-level claims below were re-verified against the real
repo (`Cargo.toml`, `msquic-h3/src/*`, `.github/workflows/build.yaml`) and the
vendored crates (`~/.cargo/registry/src/*/{msquic-2.5.1-beta,h3-0.0.8,
h3-quinn-0.0.10}`).

### MF-1 — one ordered source for order-sensitive send events

**Problem.** The narrative makes chronological enqueue order the tiebreaker
between graceful `FinishComplete` and a later `TerminalWake` ("Send-side
transitions" and the send transition table), but an earlier pre-implementation
draft routed
`FinishComplete` through a separate `shutdown` one-shot (`StreamSendCtx.shutdown`
/ `SendStreamReceiveCtx.shutdown`) while `TerminalWake` and data `Complete` ride
the `send` mpsc, and `poll_send_channel` always drains the mpsc first. A
`FinishComplete` produced before a later `TerminalWake` is therefore observed
*after* the terminal, contradicting the normative "finish already completed
wins" rule.

**Decision (Correctness > Developer Experience; keeps the data-before-finish
guarantee).** Put **all** order-sensitive send events on **one** total-order
source. Remove the separate finish one-shot: `FinishComplete { graceful }`
becomes an ordinary `SendEvent` enqueued on the same `mpsc::UnboundedSender<
SendEvent>` as `Complete { cancelled }` and `TerminalWake`. The callback and
frontend then share a single FIFO, so "finish completed before a later terminal
was enqueued" is preserved by construction, and the existing rationale that a
data `Complete` must precede the `FinishComplete` that follows it still holds
because both are on the same FIFO in submission order. (If a future revision must
keep two channels, attach a monotonic `seq: u64` at callback publication and
merge by `seq`; the single-queue form is preferred as simpler.)

Concretely: delete the `shutdown: Option<oneshot::Sender<SendEvent>>` field from
`StreamSendCtx`, the `shutdown: oneshot::Receiver<SendEvent>` field from
`SendStreamReceiveCtx`, and the second `Pin::new(&mut self.sctx.shutdown)` arm in
`poll_send_channel`; `poll_send_channel` becomes a single `poll_next_unpin` on
the one mpsc.

**Tests (acceptance criteria for the reducer/executor design, see T1).** Two
deterministic reducer tests that preload **both** orders before the frontend
polls: (a) `FinishComplete { graceful:true }` enqueued, then `TerminalWake` —
assert the finish result wins and is absorbing; (b) `TerminalWake` enqueued, then
`FinishComplete` — assert the terminal wins. Both run with no network by feeding
the single mpsc.

### MF-2 — a cancelled `SendComplete` must not sticky-mask a richer cause

**Problem.** A cancelled `Complete { cancelled:true }` can be observed before a
separate **connection**-level shutdown callback publishes `Connection(reason)`.
The reducer synthesizes sticky `Failed(QUIC_STATUS_ABORTED)` and the immutable
first-writer slot then blocks refinement to the more specific
`Stopped(code)`/`Connection(reason)`. The documented MsQuic STOP_SENDING ordering
(`PeerReceiveAborted` before send cancellation) already covers the **peer-stop**
half, but the **connection-callback** race has no such ordering.

**Decision (Correctness > implementation simplicity).** A cancelled completion
with no already-published stream/connection cause is **provisional**, not sticky:
1. If `StreamState::send_terminal` already holds `Stopped`/`Connection`, return
   it (unchanged from today).
2. Otherwise consult the **shared connection terminal** slot
   (`ConnectionTerminal`, already cloned into every stream per "Connection
   terminal publication"); if a connection reason exists there, publish and
   return `Connection(reason)`.
3. Otherwise **retain a provisional cancellation** in `SendState` (do not write
   the sticky slot yet) and refine on the paired terminal callback; only fall
   back to `Failed(ABORTED)` at a clearly specified closure point (finish/reset
   completion or stream drop) when no richer cause has appeared. An unclassified
   abort must never irreversibly outrank a later specific cause.

**Tests.** For **both** callback orders — (cancelled completion first, then
connection/peer terminal) and (terminal first, then cancelled completion) — with
an **outstanding send** in flight: peer-stop (`STOP_SENDING`) yields
`Stopped(code)`; connection shutdown yields `Connection(reason)`; only the truly
no-cause case yields `Failed(ABORTED)`. The loopback callback-order tests must
hold a send outstanding (the deterministic `CountingExec` seam can script this
without a network).

### MF-3 — reproducible, attesting native support gate

**Problem.** The unsafe close-time reclamation premise depends on the *actual
loaded* binary's inline-drain / exactly-once behavior, but the preflight only
compares `QUIC_PARAM_GLOBAL_LIBRARY_VERSION` to a single hard-coded version for
every provenance row (wrong once the rows resolve to different patch versions —
see the per-row gate in the decision below); provenance is a
`find`↔`src` edit inside `[dev-dependencies]` (an uncommitted manifest change,
not a committed CI dimension); and the `teardown` substring test filter may match
none of the named seam tests.

**Decision (Security/Reliability).**
1. **Two committed clean-checkout CI jobs, one per provenance row, selected by a
   committed feature — no working-tree manifest edit.** The `msquic` crate exposes
   mutually-exclusive `find` and `src` build features (verified:
   `~/.cargo/registry/src/index.crates.io-*/msquic-2.5.1-beta/Cargo.toml`
   `[features] find = [] / src = ["dep:cmake"]`; `scripts/build.rs` panics on
   `all(feature = "src", feature = "find")`). Today the root `Cargo.toml` hard-wires
   provenance by enabling `msquic/find` unconditionally in `[dev-dependencies]`,
   which cannot be overridden from the command line. Replace that with **two
   committed pass-through Cargo features** in `msquic-h3`'s manifest plus a plain
   `default-features = false` dev-dependency:

   ```toml
   [features]
   native-find = ["msquic/find"]   # system-package libmsquic (find::find symlinks it)
   native-src  = ["msquic/src"]    # crate-built from vendored source via cmake

   [dev-dependencies]
   msquic = { version = "2.5.1-beta", default-features = false }
   ```

   and a committed CI matrix dimension `provenance: [native-find, native-src]`,
   with every build/test step run as
   `--no-default-features --features ${{ matrix.provenance }}` so exactly one
   provenance is selected per job by committed config, never by an uncommitted
   `Cargo.toml` edit. The `native-find` job installs the pinned system `libmsquic`
   package before building; the `native-src` job builds the vendored source via the
   crate's cmake path (`scripts/build.rs` `cmake_build`). The two features are
   mutually exclusive, so a job that accidentally enables both fails fast in
   `build.rs`.
2. **Attest the library actually loaded by the running test process, not merely an
   installed package.** Both provenance rows link `libmsquic` **dynamically**
   (verified in `scripts/build.rs`: the `find` path symlinks the resolved
   `/usr/lib/x86_64-linux-gnu/libmsquic.so.2` to `$OUT_DIR/libmsquic.so`; the `src`
   path installs the shared library under `$OUT_DIR/lib`; neither is static unless
   the `static` feature is set, which these jobs do not set). The preflight
   attests the **loaded** binary two ways:
   - **Live-handle identity (primary).** Query, through the live MsQuic handle the
     test process is using, `QUIC_PARAM_GLOBAL_LIBRARY_VERSION` (must equal the
     row's pinned version — **`[2,5,8]` for `native-find`**, **`[2,5,1]` for
     `native-src`**; the as-built gate is per row, see the Phase 9 attestation
     policy) **and** `QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH` (param id `8`,
     `char[64]` null-terminated). Both values are produced by code inside the
     actually-loaded image, so they attest the running library regardless of file
     path (verified: `msquic-2.5.1-beta/src/core/library.c`
     `case QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH`; `src/inc/msquic.h`
     `#define QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH 0x01000008`; ffi const
     `QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH = 16777224` in
     `src/rs/ffi/linux_bindings.rs`; `docs/Settings.md` row 8). The captured
     version + git hash are recorded as the job's attestation for **both** rows.
   - **Loaded-artifact digest (secondary).** Resolve the shared object the process
     actually mapped — parse `/proc/self/maps` for the `libmsquic.so*` entry
     (Windows: enumerate the loaded module path) — and record that file's SHA-256.
     This ties the digest to the image the loader resolved, closing the gap the
     reviewer flagged: an installed-package digest alone does not prove the process
     loaded that file. For `native-src` the digest is the crate-built
     `$OUT_DIR/lib/libmsquic.so*`; for `native-find` it is the symlink target under
     `/usr/lib/...`, i.e. exactly what the loader resolved.
3. **Exact test targets with count/list assertions**, replacing the `teardown`
   substring filter, so a green job cannot silently run zero seam tests:

   Each command carries the committed provenance selection
   (`--no-default-features --features ${{ matrix.provenance }}`) required by the
   CI decision above, so exactly one clean-checkout provenance row is chosen per
   job — never both, never neither:

   ```sh
   cargo test -p msquic-h3 --no-default-features --features ${{ matrix.provenance }} accepted_send_reclaims_via_callback_exactly_once -- --exact
   cargo test -p msquic-h3 --no-default-features --features ${{ matrix.provenance }} immediate_send_failure_reclaims_without_completion -- --exact
   cargo test -p msquic-h3 --no-default-features --features ${{ matrix.provenance }} native_version_preflight -- --exact
   ```

   Each job asserts each named test ran exactly once (`--exact`, non-zero result
   line), and fails if any is filtered out or absent.

The security residual is scoped to *attestation and CI support claims*, not a new
runtime pointer-validation mechanism (a non-conforming library breaks the binding
generally; see Dissent Log 4).

### MF-4 — a buildable, invoked send-copy benchmark gate

**Problem.** `benches/send_copy.rs` is a separate crate and cannot call the
private `SendBuffer::new`; the release commands run only `cargo test`, never the
`harness = false` Criterion target; and the 10 ms threshold is absolute.

**Decision (Reliability/Correctness of the gate).**
1. **One buildable arrangement: a feature-gated PUBLIC bench entry point.** A
   Criterion bench (`benches/send_copy.rs`) is a separate crate and can reach only
   `pub` items, so a `pub(crate)` helper, a separate integration-test target, and
   an in-crate `#[bench]` are all rejected as the Criterion target. Instead, the
   copy path lives in the library as an **always-compiled helper** used by
   production `send_data` — `pub fn copy_into_send_buffer(src: impl Buf) -> SendBuffer`,
   declared **`pub` inside the crate-private `mod buffer`** (the module itself is
   never `pub`, so the helper is not externally reachable on its own). This is the
   *single* copy the reducer performs — production `send_data` calls
   `copy_into_send_buffer(&mut wb)` and the benchmark calls
   `copy_into_send_buffer(&src[..])`, so both drive the identical
   `src.copy_to_bytes(remaining)` allocation — and the library re-exports the helper
   **publicly only under a committed bench feature**. A `pub use` can only re-export
   an item that is itself `pub`, so the helper must be `pub` (not `pub(crate)`)
   within its private module. Its return type must also be `pub` or the public
   re-export trips `private_interfaces` under the repo's `-D warnings`; the type is
   made `pub struct SendBuffer` **without** re-exporting its name, so it is legal in
   the public signature yet remains unnameable outside the crate (a bench can *bind*
   the returned value by inference but cannot write the path `buffer::SendBuffer`):

   ```rust
   // msquic-h3/src/buffer.rs — `mod buffer;` is crate-private, but both the helper
   // and its return type are `pub` so a gated `pub use` can re-export the helper
   // (a `pub(crate)`/private `fn`, or a `pub fn` returning a private type, could
   // NOT be re-exported publicly by bench_support without a `private_interfaces`
   // error under `-D warnings`).
   use bytes::Buf;

   pub struct SendBuffer { /* buffers: [BufferRef; 1], _bytes: Bytes */ }

   /// The ONE production copy path: consume the complete `Buf` into owned `Bytes`.
   /// `&mut WriteBuf<B>` (production) and `&[u8]` (bench) both implement `Buf`, so
   /// both callers run the identical `copy_to_bytes` allocation.
   pub fn copy_into_send_buffer(mut src: impl Buf) -> SendBuffer {
       let n = src.remaining();
       SendBuffer::new(src.copy_to_bytes(n))   // fresh owned allocation copy
   }

   // msquic-h3/src/lib.rs
   mod buffer;   // private module (no `pub`): helper + type stay unreachable by default
   #[cfg(feature = "bench-internals")]
   pub mod bench_support {
       // Re-exports ONLY the function; `SendBuffer`'s name is never re-exported.
       pub use crate::buffer::copy_into_send_buffer;
   }
   ```

   with the feature, the Criterion dev-dependency, and the bench target declared in
   the committed manifest:

   ```toml
   [features]
   bench-internals = []

   [dev-dependencies]
   criterion = "0.5"

   [[bench]]
   name = "send_copy"
   harness = false
   required-features = ["bench-internals"]
   ```

   The production `send_data` path always calls the same private helper
   `copy_into_send_buffer(&mut wb)` (no feature needed) — so the benchmark measures
   the identical copy code, not a second implementation; the public surface appears
   **only** when `bench-internals` is enabled, so the bench compiles against a `pub`
   entry point without leaking bench-only API into the default build.
   `required-features` keeps the bench out of ordinary `cargo test`/`cargo bench`
   runs that do not opt in.
2. **Exact blocking CI invocation.** Add a dedicated CI step (today
   `.github/workflows/build.yaml` runs only `check`/`fmt`/`clippy`/`test`). The
   benchmark job runs under the same `provenance` matrix as the other conformance
   steps, so its command carries **both** the matrix provenance **and**
   `bench-internals` (cargo accepts a comma-separated feature list), never a
   provenance-less build:

   ```sh
   cargo bench -p msquic-h3 --no-default-features --features ${{ matrix.provenance }},bench-internals --bench send_copy -- --noplot
   ```
3. **Split the gates.** Keep the deterministic allocation/peak-resident bound
   (`peak_delta <= 2*payload + 1 MiB`) as a blocking in-crate **`#[test]`** (private
   access, no feature needed), so correctness does not depend on Criterion timing.
   The blocking Criterion bench therefore carries **no hard timing assertion**:
   treat copy **timing** as *informational / relative to a same-run baseline* unless
   the runner is concretely pinned and isolated; the median-of-64 policy is retained
   as partial scheduler-noise control, but the absolute 10 ms number is advisory
   until a pinned runner exists.

### MF-5 / T2 — resolve the 16 MiB ceiling before ratifying it

**Problem.** The 16 MiB cap bounds one allocation but not (a) synchronous
executor blocking — the copy runs on the polling thread
(`copy_into_send_buffer(&mut wb)`, i.e. `wb.copy_to_bytes(n)`) — nor (b) aggregate
memory across concurrent streams, since the cap is per-operation and allocation is
infallible/abort-on-OOM. A single-copy 10 ms benchmark cannot establish a safe
default under concurrency.

**Decision (Reliability + Performance > compatibility convenience). 16 MiB is
PROVISIONAL and is not ratified from the single-copy benchmark alone.** Prefer
**internal chunking** with an explicit **per-connection in-flight byte budget**;
if chunking stays deferred, **lower the cap** and document + test a maximum
concurrent-memory assumption. The decision table below must be filled with real
multi-stream measurements before a value is committed:

| Option | Per-op cap | Aggregate bound | Scheduler latency | Complexity | Compatibility |
| --- | --- | --- | --- | --- | --- |
| Keep 16 MiB (current) | 16 MiB | **unbounded** across N streams | up to one 16 MiB copy on the poll thread | low | best (one native send per `send_data`) |
| Lower cap (e.g. 1 MiB) | 1 MiB | N × 1 MiB (still per-stream) | small | low | may split large app frames |
| Internal chunking + per-conn budget | small chunk | **bounded** by the budget | bounded per chunk | high (reducer tracks many native sends per `send_data`) | transparent to callers |

Measurements to add before committing: multi-stream latency (concurrent large
sends) and aggregate retained bytes, not only the single 16 MiB copy. Until the
table is filled and a value chosen, `MAX_ADAPTER_SEND` is documented as
provisional (see the finding-5 text and the as-built `MAX_ADAPTER_SEND` constant).

### MF-6 — release, versioning, migration, and rollback

**Problem.** The change removes public `H3Buff`, adds an `OversizedSend`
rejection, and changes downstream-visible terminal classifications, but the
implementation order had no compatibility/version decision, changelog, migration
note, or rollback path. Verified: `H3Buff` is `pub` (`msquic-h3/src/buffer.rs`
`pub struct H3Buff`), the crate is `version = "0.0.6"` (`Cargo.toml`).

**Decision (scoped for a pre-1.0 `0.0.x` crate: the first two items are
blocking; consumer-validation and formal rollback are recommended, not
blocking).**
1. **Version decision.** Removing `pub H3Buff` and adding new runtime errors are
   breaking API/behavior changes. Under Cargo pre-1.0 semantics, `0.0.z` bumps
   are all breaking, so publish as **`0.0.7`** and record the decision. (Note:
   `msquic-h3-0.0.5` and `-0.0.6` already exist in the local registry, so `0.0.7`
   is the next free slot.)
2. **Changelog + migration notes.** Add a `CHANGELOG.md` entry documenting:
   removal of `pub H3Buff` (consumers must stop naming it; the owned `SendBuffer`
   is internal), the new `StreamErrorIncoming::Unknown(OversizedSend)` for sends
   above `MAX_ADAPTER_SEND`, and the changed terminal classifications (peer
   reset → `StreamTerminated`, connection close → specific
   `ConnectionErrorIncoming` instead of `ApplicationClose { 0 }`, peer
   `STOP_SENDING` surfaced). Migration note: callers relying on the old
   "reset-as-EOF" or "always application-close-0" behavior must update error
   handling.
3. **Recommended (non-blocking at 0.0.x):** prerelease validation against a
   representative h3 consumer, and a rollback note — revert to `0.0.6` and the
   last-known-good native/config matrix row if a regression is found. These are
   proportionate to record but not gates for an unstable-API crate with no
   evident external consumers.

### MF-7 / T5 — the appendix is a temporary scaffold, not a second source of truth

**Decision (Maintainability, once code exists).** The ~1,800-line appendix was
demoted from "normative / takes precedence" to a **temporary pre-implementation
scaffold** and, now that the implementation has landed (Phases 1–9 in
`msquic-h3/src/`), **has been removed per its expiry condition** (Phase 10 /
implementation-order item 10). The living design of record is this narrative plus
the resolutions section — invariants, interfaces, mappings, and rationale — and
the as-built reference in `.paw/work/error-propagation/Docs.md`. No depended-on
invariant was deleted: the load-bearing rules (the send transition table below,
the single-ordered send-event queue of MF-1, the exactly-once reclamation
contract of SF-3, and the per-row native-version gate of MF-3) all live in the
narrative and are realized in the checked production code and tests. See the
"As-built implementation" section where the scaffold formerly stood.

### SF-1 — name `StreamExecutor` as the send-half owner

Resolved in place: the `H3SendStream` ownership comment now states that the
production `StreamExecutor` (held by `exec`) owns the send-side
`Arc<msquic::Stream>` **independently** of the recv half, and that dropping
`H3RecvStream` first does not invalidate the send side (each half holds its own
`Arc` clone). **Test:** drop `H3RecvStream`, then use and drop `H3SendStream`;
assert send-side verbs still function and the native handle is released only when
the last owner drops.

### SF-2 — guard `finish_started` before the destructive poll

**Problem.** `poll_ready` polls the event sources (consuming the one-shot
`FinishComplete`) *before* the reducer rejects the call for `finish_started`, so
a misuse `poll_ready`-after-finish destroys the only finish completion and a
later `poll_finish` hangs. (Note: with MF-1's single queue there is no separate
finish one-shot, but the same destructive-consume hazard applies to any consumed
event.)

**Decision (Correctness/liveness).** Check the non-consuming state guard
**before** polling:

```rust
if self.sctx.reducer.finish_started {
    return Poll::Ready(Err(poll_ready_after_finish_error()));
}
let poll = self.poll_send_channel(cx);
```

Alternatively, retain any consumed event until the method that owns it processes
it. **Test:** call `poll_ready` after finish has started, then `poll_finish`, and
assert `poll_finish` still completes (no lost `FinishComplete`, no hang).

### SF-3 — prove exactly-once `SendBuffer` destruction on the real path

**Decision.** Factor production boxing/reclamation into a package-private helper
used by **both** `StreamExecutor` and `CountingExec` (removing the hand-mirrored
allocation in `CountingExec`), and add a `#[cfg(test)]` drop counter carried by
`SendBuffer` (e.g. an `Arc<AtomicUsize>` incremented in `Drop`). **Tests:** after
callback-replayed `SendComplete`, assert `drops == 1`; after immediate
`Stream::send` failure reclamation, assert `drops == 1`. This observes the real
destructor, not queue behavior.

### SF-4 — the appendix is not claimed compilable

Resolved via MF-7/T5: the appendix banner no longer claims "concrete,
**compilable**, normative" Rust; it is labeled a scaffold. If any fragment is
later promoted to a checked fixture, it must compile under the repo's `clippy -D
warnings` policy (fixing the transitive `Debug` derives and the unused `start_rx`
in `stream_ctx_channel`). Until then the compilability claim is dropped rather
than asserted.

### SF-5 — bounded per-stream receive backpressure (implemented)

**Decision (superseded — now implemented, not deferred).** The earlier
down-scoped decision (document the trust assumption, defer the redesign) has been
**superseded by the SF-A per-stream backpressure fix** (see "Per-stream receive
backpressure" under Receive-side transitions). Receive buffering is now bounded
**per stream** by `MAX_RECV_BUFFER` (1 MiB): a stream whose buffered bytes reach
the bound returns `QUIC_STATUS_PENDING` and is re-armed via `receive_complete`
only after the reader drains below the bound, so a peer can no longer enqueue
arbitrary peer-controlled bytes ahead of a stalled h3 reader on a single stream.
Connection memory is bounded by the per-stream bound × the negotiated max
concurrent streams. Deployment note: the bound is per stream, so total resident
receive memory still scales with the concurrent-stream count (itself governed by
transport/stream flow control).

### SF-6 — define the post-`stop_sending` polling contract

**Decision (edge-cases; the contract is required and is fixed to `Ok(None)`).**
After local `stop_sending` (which submits `ABORT_RECEIVE` and, per the
`stop_sending(code)` rule above, injects **no** receive terminal), the retained
receive half sets a **local, sticky `receive_closed` flag** that is distinct from
the stored `StreamErrorIncoming` terminal-reason slot — that slot is left
untouched. Any subsequent `poll_data` returns `Poll::Ready(Ok(None))`, the
verified end-of-stream signal: `h3::quic::RecvStream::poll_data` returns
`Poll<Result<Option<Self::Buf>, StreamErrorIncoming>>` and its doc comment states
that `None` means "the receiving side will no longer receive more data" (checked in
`~/.cargo/registry/src/index.crates.io-*/h3-0.0.8/src/quic.rs`). `Ok(None)` is
deliberately **not** a `StreamErrorIncoming` variant, so this contract injects no
receive terminal and stays consistent with the earlier "do not attempt to inject a
receive terminal" rule at the `stop_sending(code)` paragraph above. A
`StreamTerminated { error_code }` would be the wrong choice here: that variant is
documented in `h3-0.0.8/src/quic.rs` as "Stream side was closed by the peer"
(a peer reset / peer `stop_sending`), not our local decision. Polling after
`stop_sending` is therefore neither an indefinite hang (`Pending`) nor treated as
misuse: it is a defined clean end-of-stream. **Test:** call `stop_sending`, then
`poll_data` with no peer FIN/reset and an open connection; assert
`Poll::Ready(Ok(None))` (not `Pending`, not `Err`), and assert the stored
receive terminal-reason slot remains empty (no terminal was injected).

### SF-7 / T4 — precedence/refinement for connection terminal, not first-observed

**Problem.** "First writer wins" is first-*observed* by callback scheduling, not
first-*caused* or most-specific; a provisional `LocalClose` published before the
shutdown downcall can permanently mask a nearly-simultaneous peer/transport
cause.

**Decision (Correctness > simplicity).** Define an explicit
precedence/refinement rule: **provisional** classes (`LocalClose`, generic
fallback) may be **refined** to a **specific** peer/transport cause
(`PeerApplication`, `Timeout`, `Transport`) until the terminal becomes
**externally observable** (delivered to h3 via a poll or an accept frontend);
after that freeze point the winner is immutable. Specific causes never refine to
provisional ones. **Test:** a controlled simultaneous-close test (local close
racing a peer/transport shutdown callback) asserting the specific cause wins when
it is available before external observation, and immutability after.

### SF-8 — a real contention test for the first-writer invariant

**Decision.** Add a real-thread (or `loom` model) test that releases **two**
publishers simultaneously against the poison-safe publication helper, repeats
enough iterations to cover both winners, and asserts: exactly one immutable
winner per scope, all readers observe the same value, and send/receive scopes
remain independent. This complements the sequential reducer tests (which cannot
exercise contention or poison recovery).

### SF-9 — one authoritative connection-terminal representation

**Decision.** Declare the **shared mutex slot** (`ConnectionTerminal` behind the
`Arc<Mutex<Option<..>>>`) **authoritative for identity**. The
`IncomingStream::Terminal` channel payloads and each receiver's sticky cache are
**derived**: channel entries act only as ordered wake/barrier markers that
preserve the queued-stream-before-terminal ordering, and per-receiver caches are
immutable snapshots of the shared winner taken at first observation (avoiding
relocking). Documented invariant: a cache/payload is only ever populated from the
shared winner, never independently, so the three representations cannot diverge;
the queue-order contract (streams queued before the terminal are delivered first)
is why the payload copy exists (see Dissent Log 5).

### SF-10 — version + symbol anchors for vendored citations

**Decision (maintainability hygiene, applied going forward).** New vendored-source
evidence names the **symbol/function + pinned version** and treats bare line
numbers as secondary, matching the doc's existing good practice (the versioned
h3/h3-quinn doc links and the v2.5.1 `StreamClose.md` permalink). Example
conversions for the called-out spots: cite
`msquic 2.5.1-beta: QuicStreamSendBufferRequest` (in `src/core/stream_send.c`)
and `msquic 2.5.1-beta: QUIC_PARAM_GLOBAL_LIBRARY_VERSION` (in
`ffi/linux_bindings.rs`) rather than bare line numbers; line numbers stay only as
a local convenience. This applies to all citations added by future edits; the
existing in-tree vendored line numbers are stable until the vendored copy is
bumped, so they are updated opportunistically, not en masse.

### Trade-offs (adopted AUTO decisions)

- **T1 — keep the reducer/executor design.** Correctness and Reliability outrank
  structural consistency and Developer Experience. The abstraction is retained
  **only** with MF-1, MF-2, SF-2, and SF-8 as explicit acceptance-test criteria
  (this section defines them). Decision record: the reducer/executor seam earns
  its divergence from the current inline style by giving a testable boundary for
  synchronous callbacks, lost-waker handling, and exactly-once native commands.
- **T2 — 16 MiB not approved from the benchmark alone;** prefer chunking, else
  lower the cap. Recorded under MF-5 above.
- **T3 / C-9 — retain `SendBuffer`.** Correctness and Security outrank the copy
  reduction. Removing the owned copy is a **future optimization** gated on
  proving, for **every** supported binary, provenance row, configuration, and
  accepted payload path: (1) native copies synchronously for all supported send
  modes, (2) completion ordering is stable, (3) loaded-artifact identity is
  attested (MF-3), and (4) zero-copy lifetime tests cover every support row; the
  native identity and teardown gates must be re-run before removal. Verified
  context: buffered sends copy into an internal buffer and indicate
  `SEND_COMPLETE` synchronously (`msquic 2.5.1-beta: QuicStreamSendBufferRequest`),
  but this holds only for the examined buffered path, so it is not yet a proof
  across the matrix.
- **T4 — refine toward the specific peer/transport cause.** Recorded under SF-7
  above.
- **T5 — appendix is a temporary artifact.** Recorded under MF-7 above.

### Consider (C-1..C-9)

- **C-1 — resolved in place:** no-reason stream-start cancellation maps to nested
  `ConnectionErrorIncoming::InternalError` (not `Unknown`); the finding-6 prose
  was corrected.
- **C-2 — add a local-close mapping test:** assert local connection close maps to
  `Undefined(LocalConnectionClose)`, is **not** `ApplicationClose { 0 }`, and
  preserves the adapter-owned error's display text. (Guards the exact current bug
  where every close became `ApplicationClose { 0 }`.)
- **C-3 — add stream-ID boundary tests:** `validate_stream_id` over `0`,
  `(1<<62)-1`, `1<<62`, and `u64::MAX`, plus separate accepted-stream query
  failure vs. conversion failure cases.
- **C-4 — loopback determinism controls:** specify the Tokio runtime flavor,
  ephemeral-port allocation, per-test deadlines, a no-retry failure policy, and
  which race-sensitive tests (idle-timeout, callback-order) require serialization
  or per-connection controls.
- **C-5 — keep invariant tests independent of table rows:** retain a small
  invariant suite (exactly-once submission, absorbing success, terminal
  consistency) whose expectations are authored independently of the transition
  table so a wrong row and its derived test cannot agree.
- **C-6 — clarify opener clone with a pending start:** document reachability. If
  cloning a `StreamOpener` while an `OpeningStream` owns a started native stream
  is unreachable in the h3 usage model, state that (one-line "not reachable
  because…"); if reachable, prohibit it, share the pending op, or cancel with a
  meaningful code (not silent abort-code-0) and test the path.
- **C-7 — add an h3 dependency-bump gate:** before changing `h3 = "0.0.8"`,
  require semantic review of the QUIC traits/error variants, the
  `i-implement-a-third-party-backend-and-opt-into-breaking-changes` feature
  exposure (verified present in `h3-0.0.8/Cargo.toml`), mapping tests, and
  compile checks — symmetric with the MsQuic bump gate.
- **C-8 — add a sanitizer/leak signal:** add an available ASan/LSan (or
  platform leak-check) job around loopback teardown and the seam tests, clearly
  documented as **complementary** evidence, not a substitute for the native
  source review of the exact inline-drain path.
- **C-9 — consolidated into T3** (proof obligations for removing `SendBuffer`).

## As-built implementation

> The ~1,800-line "Implementation-ready definitions (round 6)" appendix that
> formerly stood here was a **temporary pre-implementation scaffold** (MF-7 / T5).
> Now that the redesign has landed, it has been **removed per its expiry
> condition** (Phase 10 / implementation-order item 10). The concrete, checked
> definitions live in the production code under `msquic-h3/src/` and its inline
> tests; the durable as-built technical reference is
> [`.paw/work/error-propagation/Docs.md`](../.paw/work/error-propagation/Docs.md).
> This section promotes the load-bearing invariants the design depends on so
> nothing is lost with the scaffold.

**Where the code lives (as built).**

- `msquic-h3/src/error.rs` — the scoped terminal vocabulary
  (`ConnectionTerminal`, `ReceiveTerminal`, `SendTerminal`), the `convert_*`
  minting boundary, outgoing application-code clamping (`clamp_application_code`,
  `MAX_QUIC_VARINT`), the send-size ceiling (`MAX_ADAPTER_SEND`, `OversizedSend`),
  and the pure send reducer (`transition`, `SendInput`/`SendCommand`).
- `msquic-h3/src/buffer.rs` — the owned `SendBuffer`, `classify_send_len`, the
  single production copy path (`copy_into_send_buffer`), and the exactly-once
  reclamation `Drop`.
- `msquic-h3/src/lib.rs` — the FFI callbacks (with the SF-E `guard_callback`
  panic-containment backstop), the connection terminal slot with refinement and
  commit-on-delivery freeze, the explicit receive events with per-stream
  `MAX_RECV_BUFFER` backpressure (SF-A), safe stream open/identity, and the
  `SendExec`/`OpenExec` executor seam plus the conformance suite.
- `msquic-h3/src/attest.rs` — the per-row native-version attestation gate.

**Promoted invariants (design-load-bearing; realized in the code above).**

1. **Single ordered send-event source (MF-1).** All order-sensitive send events
   (data completion, terminal wake, finish completion) ride one mpsc, so
   chronological finish-vs-terminal order is preserved by construction. There is
   no separate finish one-shot to race.
2. **Pure send reducer over a native-free state machine.** `transition` mutates
   only `SendState` and emits exactly one `SendCommand` per input; the frontend
   executor runs the command against MsQuic through the `SendExec` seam and feeds
   the result back. The send transition table above (see "Send transition table")
   is the authoritative specification of these transitions.
3. **`ProvisionalAbort` refinement (MF-2).** A cancelled `SendComplete` with no
   yet-known cause is recorded as the distinct provisional `SendTerminal::ProvisionalAbort`
   marker, never surfaced while unobserved, refinable to a richer peer/connection
   cause, and finalized to `Failed(QUIC_STATUS_ABORTED)` at the defined closure
   point. A real native `Failed` (including `QUIC_STATUS_ABORTED`) is authoritative
   and never refined.
4. **First-writer-wins terminal scopes with bounded refinement (SF-7 / T4,
   SF-9).** Each scope (connection/receive/send) records its terminal reason
   once. A *provisional* cause (a `ConnectionTerminal::LocalClose`, or the send
   `ProvisionalAbort` marker) may be refined to a more-specific peer/transport
   cause until a cause is actually **delivered** to a caller (commit-on-delivery,
   via `commit_conn`); delivery freezes the value. A **non-delivery** — an empty
   slot returning a synthetic internal/unknown error — never freezes, so a later
   real cause can still be recorded and delivered. `reset()` and a graceful finish
   deliver no observable terminal and therefore never freeze the connection slot.
5. **Exactly-once `SendBuffer` reclamation (SF-3).** The owned buffer is destroyed
   exactly once — either by the callback-replayed `SendComplete` reclaiming the
   leaked box, or by the caller on an immediate `Stream::send` failure — proven by
   the `Drop` counter in the seam tests, never twice and never leaked.
6. **Explicit receive events, no empty-non-FIN EOF.** Data, FIN, peer reset,
   local `stop_sending`, and connection failure are distinct events; an empty,
   non-FIN receive notification produces no event (it is not EOF). A clean FIN
   maps to `Ok(None)`; a peer reset maps to `StreamTerminated`.
7. **Per-row native-version attestation gate (MF-3).** The version gate is
   **row-specific**: `native-find` (system-package libmsquic) expects `[2,5,8]`;
   `native-src` (crate-built from the vendored `msquic-2.5.1-beta` source) expects
   `[2,5,1]`; `MSQUIC_EXPECTED_VERSION` overrides per matrix cell. On Linux the
   loaded-artifact digest (from `/proc/self/maps`) is a **blocking** part of the
   gate. This supersedes any earlier single hard-coded `[2,5,1]` pin.
8. **Close-time inline-drain is source-review-only (SC-007).** The unsafe
   close-time reclamation premise is established by native-source review plus the
   binding's uniform `close_inner` contract, **not** by an executable
   drop-triggered teardown test (the public API cannot hold a real send stream at
   drop). It is documented as a labelled, untested guarantee.
9. **Provenance selection and docs.rs (SF-L).** Native-library provenance is a
   committed, mutually-exclusive feature: `native-find` (system-package
   libmsquic) or `native-src` (self-contained via vendored source built by
   cmake). docs.rs builds under `native-src` (pinned in
   `[package.metadata.docs.rs]` with `--cfg docsrs`, and the `docsrs` cfg is
   registered via `[lints.rust] check-cfg` so `-D warnings` stays clean). A
   crate-level `compile_error!` rejects the **neither-enabled** misconfiguration
   with an actionable message whenever neither provenance is selected
   (unconditionally — no `docsrs` exemption; docs.rs is unaffected because its
   metadata selects `native-src`, so the guard never fires there); the
   **both-enabled** case is bounded by the upstream `msquic` build-script panic
   (`feature src and find are mutually exclusive`), not intercepted at the crate
   level. `--all-features` is therefore never a valid success gate.

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
8. Add the deterministic `CountingExec` send-seam tests (outstanding-send
   retain/reclaim and immediate-failure reclaim), then add the loopback
   accepted-send reclamation-ordering conformance test. Close-time inline-drain
   compatibility is covered by native-source review plus the binding's uniform
   `close_inner` close contract, not by a drop-triggered teardown test (none
   exists).
9. Add runtime native-version verification, the supported matrix, and send-copy
   performance/peak-memory results to `Development.md`.
10. **Release, compatibility, and scaffold cleanup gate (resolves MF-6 and the
    MF-7/T5 expiry condition).** Before publishing, complete the compatibility
    and rollback checklist in the "MF-6 — release, versioning, migration, and
    rollback" resolution above (crate-version decision, `CHANGELOG.md` entry,
    migration notes), and execute the appendix cleanup (delete/archive the
    ~1,800-line appendix, promote any depended-on invariant into the narrative).
    Both are done in Phase 10: the crate is bumped to `0.0.7` with a
    `CHANGELOG.md`, and the appendix has been replaced by the "As-built
    implementation" section above.

This order fixes callback safety and the error vocabulary first, then moves each
callback path onto it while keeping changes reviewable and bisectable.