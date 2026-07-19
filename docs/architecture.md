# Architecture

`msquic-h3` is a thin adapter that lets an application drive
[Microsoft's msquic](https://github.com/microsoft/msquic) QUIC stack through the
[`h3`](https://github.com/hyperium/h3) HTTP/3 transport traits. It owns no
protocol logic of its own: h3 provides the HTTP/3 state machine, msquic provides
QUIC, and this crate is the translation layer between msquic's C FFI callbacks
and the `h3::quic` trait surface that h3 polls.

This document is the map of the library. Each subsystem has its own deep-dive:

- [Error model](./error-model.md) — how QUIC terminal conditions become `h3`
  error values, the adapter-owned terminal enums, and the pure send reducer.
- [Receive and send](./receive-and-send.md) — per-stream receive backpressure and
  the owned-buffer send path.
- [Callback safety](./callback-safety.md) — panic containment and ownership/scope
  discipline at the FFI boundary.
- [Registration lifecycle](./registration-lifecycle.md) — registration rundown
  tracking and `wait_idle`.
- [Testing](./testing.md) — test seams, the loopback conformance suite, and the
  native-library attestation gate.
- [Development](./Development.md) — how to build, test, and run locally.

## Purpose and boundary

h3 defines a small set of transport traits (`h3::quic::Connection`,
`OpenStreams`, `SendStream`, `RecvStream`, `BidiStream`). The adapter implements
those traits over msquic handles and translates every msquic event into the
value h3 expects.

The trait boundary is important because it defines what this crate is responsible
for. Errors that occur *before* a connection is handed to h3 — constructing a
`Registration`, a configuration, a `Listener`, or dialing a `Connection` — are
ordinary `Result<_, msquic::Status>` values and are not part of the h3 transport
contract. Everything *after* h3 owns the connection flows through the traits, and
that is where the terminal-cause machinery in the [error model](./error-model.md)
lives.

## Module map

The crate root (`../msquic-h3/src/lib.rs`) is a small module that wires the
submodules together (module declarations, crate-root re-exports, a few shared
prelude helpers, and the `msquic`/`bench_support` re-exports). The adapter's logic
lives in the sibling modules below.

| Module | Role |
| --- | --- |
| `lib` (`../msquic-h3/src/lib.rs`) | Crate-root wiring: module declarations, public/`pub(crate)` re-exports, shared prelude helpers (`lock_recover`, `H3_INTERNAL_ERROR`, `internal_error_status`). |
| `callback` (`../msquic-h3/src/callback.rs`) | Callback-safety primitives: `guard_callback`, `PoisonFlag`, `ShutdownSeam`, `CbClass`, `ForceShutdown`. |
| `terminal` (`../msquic-h3/src/terminal.rs`) | Terminal-cause machinery: the connection/send terminal slots, `ConnTerminalState`, commit-on-delivery + refinement helpers, transport/shutdown classification. |
| `connection` (`../msquic-h3/src/connection.rs`) | `Connection`, `ConnHandle`, the connection FFI callback + recovery, the accept path, `ConnectionShutdownWaiter`, and the `Connection` h3 trait impls. |
| `opener` (`../msquic-h3/src/opener.rs`) | `StreamOpener` and its `OpenStreams` impl, stream-open outcome classification. |
| `stream` (`../msquic-h3/src/stream.rs`) | The stream types (`H3Stream`/`H3SendStream`/`H3RecvStream`), the send/receive exec seams, the owned-send buffer transfer, receive backpressure, the stream FFI callback, and the stream h3 trait impls. |
| `error` (`../msquic-h3/src/error.rs`) | Adapter error vocabulary, the scoped terminal enums, the h3-conversion helpers, and the pure send reducer `transition()`. |
| `buffer` (`../msquic-h3/src/buffer.rs`) | The owned send payload (`SendBuffer`), send-length classification, and the single production copy path. |
| `listener` (`../msquic-h3/src/listener.rs`) | The server-side `Listener` plus the listener FFI callback and its ownership-aware recovery. |
| `registration` (`../msquic-h3/src/registration.rs`) | `Registration`, rundown tracking (`RundownState`/`RundownGuard`), and the `WaitIdle` future. |
| `attest` (`../msquic-h3/src/attest.rs`, test-only) | Runtime native-library version / git-hash / digest attestation gate. |

Two feature-gated re-exports round out the surface: `bench_support` (under the
`bench-internals` feature) re-exports the send copy path for the Criterion
benchmark, and `pub use ::msquic::*` re-exports the upstream crate so callers can
name msquic types without a second dependency.

## Public API surface

The public surface is intentionally small. Types come from the modules noted:

- **Connection & streams**: `Connection` (`../msquic-h3/src/connection.rs`),
  `ConnectionShutdownWaiter`, `StreamOpener`, and the stream types `H3Stream`,
  `H3SendStream`, `H3RecvStream`. `H3Stream` composes a send half and a receive
  half and can `split` into the two.
- **Server**: `Listener` (`../msquic-h3/src/listener.rs`) — the accept API. It is
  *not* an h3 trait impl; it produces `Connection`s that are.
- **Registration**: `Registration` and `WaitIdle`
  (`../msquic-h3/src/registration.rs`).
- **Error types**: the adapter-owned `MsQuicTransportError`, `LocalConnectionClose`,
  `LocalStreamReset`, and `OversizedSend` (`../msquic-h3/src/error.rs`), described
  in the [error model](./error-model.md).
- **Re-exports**: `msquic` (the whole upstream crate) and, under `bench-internals`,
  `bench_support`.

The `error` module also defines two crate-internal constants — `MAX_QUIC_VARINT`
(the QUIC 62-bit varint ceiling used to clamp outgoing application codes) and
`MAX_ADAPTER_SEND` (the provisional 16 MiB per-`send_data` ceiling). They are
`pub` within the private `error` module but are not re-exported, so they are not
nameable outside the crate; they are covered in the [error model](./error-model.md)
and [receive and send](./receive-and-send.md).

`SendBuffer` is likewise a `pub` item inside the **private** `buffer` module, so
it is not nameable outside the crate; it is an internal type documented in
[receive and send](./receive-and-send.md).

## FFI / callback model

msquic delivers connection, stream, and listener events by invoking C function
pointers. The adapter registers a Rust closure for each handle and dispatches on
the event. There are exactly three callbacks, and every one runs inside
`guard_callback` (`../msquic-h3/src/callback.rs`), the panic backstop described in
[callback safety](./callback-safety.md):

- **`connection_callback`** (`../msquic-h3/src/connection.rs`) — handles
  `Connected`, `ShutdownInitiatedByPeer`, `ShutdownInitiatedByTransport`,
  `PeerStreamStarted`, and `ShutdownComplete`. Registered when a connection is
  opened (client) or attached (server-accepted).
- **`stream_callback`** (`../msquic-h3/src/stream.rs`) — handles `StartComplete`,
  `SendComplete`, `Receive`, `PeerSendShutdown`, `PeerSendAborted`,
  `PeerReceiveAborted`, `SendShutdownComplete`, and `ShutdownComplete`. Registered
  when a stream is attached or opened-and-started.
- **`listener_callback`** (`../msquic-h3/src/listener.rs`) — handles
  `NewConnection` and `StopComplete`. Registered by `Listener::new`.

Callbacks communicate with the polling side (the h3 traits) through per-handle
context structs that carry channels, wakers, and terminal-cause slots. Ordered
events ride single-producer channels; the shared terminal-cause slots and the
per-stream receive budget are guarded by narrowly-scoped mutexes rather than one
connection-wide lock. The terminal-cause slots and their first-writer /
commit-on-delivery discipline are the subject of the [error model](./error-model.md).

## h3 trait mapping

| h3 trait | Adapter type | Notes |
| --- | --- | --- |
| `h3::quic::Connection<B>` | `Connection` | `RecvStream = H3RecvStream`, `OpenStreams = StreamOpener`; drives `poll_accept_recv` / `poll_accept_bidi`. |
| `OpenStreams<B>` | `StreamOpener` | `BidiStream = H3Stream`, `SendStream = H3SendStream`; `poll_open_bidi` / `poll_open_send` / `close`. |
| `OpenStreams<B>` | `Connection` | A convenience bypass delegating to the connection's own `StreamOpener`. |
| `SendStream<B>` | `H3SendStream` | `poll_ready`, `send_data`, `poll_finish`, `reset`, `send_id`. |
| `RecvStream` | `H3RecvStream` | `Buf = Bytes`; `poll_data`, `stop_sending`, `recv_id`. |
| `SendStream<B>` / `RecvStream` / `BidiStream<B>` | `H3Stream` | Delegates to its send/receive halves; `split` yields `(H3SendStream, H3RecvStream)`. |

The send-side trait methods are driven by the pure reducer described in the
[error model](./error-model.md); the receive-side methods drive the backpressure
cycle described in [receive and send](./receive-and-send.md).

## Where to go next

- To understand how a peer close, reset, or `STOP_SENDING` reaches h3 as the right
  error variant, read the [error model](./error-model.md).
- To understand flow control and buffer ownership, read
  [receive and send](./receive-and-send.md).
- To understand why a panic in a callback cannot corrupt msquic's state, read
  [callback safety](./callback-safety.md).
