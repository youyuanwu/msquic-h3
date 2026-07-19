# Error model

This document describes how `msquic-h3` maps QUIC terminal conditions into the
`h3` transport error types, the adapter-owned vocabulary it uses to record *why*
a connection or stream ended, and the pure state machine that drives the send
half. It covers everything that happens after a connection has been handed to
h3; connection setup errors (registration, configuration, listener, dial) are
ordinary `Result<_, msquic::Status>` values outside the h3 boundary — see the
[architecture overview](./architecture.md).

The adapter exposes the information needed for useful h3 errors, but the core
challenge is *state propagation*: a msquic callback records a terminal reason,
and a later poll on the h3 side must recover the correct reason rather than
seeing a bare "channel closed". Closing a channel alone cannot distinguish a
clean FIN from a peer reset, a connection close, or an internal adapter failure,
so the adapter carries an explicit terminal cause alongside its data and
completion signals.

## The h3 error contract

`h3::quic` defines two incoming error levels. The connection-scope conversion is
centralized in the helpers in `../msquic-h3/src/error.rs` (`convert_conn`,
`convert_send`, `convert_recv`, `convert_send_op`), which are the only places a
`ConnectionTerminal` becomes a `ConnectionErrorIncoming`. The stream-open and
accept frontends in `../msquic-h3/src/connection.rs` and `../msquic-h3/src/opener.rs`
also construct a few
`StreamErrorIncoming::ConnectionErrorIncoming` / `Unknown` wrappers directly, but
they still route any connection cause through that same connection conversion:

| h3 variant | Meaning |
| --- | --- |
| `ConnectionErrorIncoming::ApplicationClose { error_code }` | The peer closed the QUIC connection with an HTTP/3 application code. |
| `ConnectionErrorIncoming::Timeout` | The QUIC connection timed out. |
| `ConnectionErrorIncoming::InternalError` | The adapter violated its own contract or failed internally. h3 closes with `H3_INTERNAL_ERROR` (`0x0102`). |
| `ConnectionErrorIncoming::Undefined` | A transport or implementation error that h3 cannot classify. |
| `StreamErrorIncoming::ConnectionErrorIncoming` | A stream operation failed because the connection failed. |
| `StreamErrorIncoming::StreamTerminated { error_code }` | The peer reset its send side, or sent `STOP_SENDING` for the local send side. |
| `StreamErrorIncoming::Unknown` | An unclassified stream-local failure. h3 handles it like stream termination. |

`Ok(None)` from `RecvStream::poll_data` is distinct: it means the peer finished
its sending side cleanly. It must never stand in for `RESET_STREAM` or a
connection failure — conflating them is the class of bug this model exists to
prevent.

Outgoing application error codes (from `reset` / `stop_sending`) are clamped to
`MAX_QUIC_VARINT` (the QUIC 62-bit varint ceiling) by `clamp_application_code`
before they reach msquic, which rejects a code that will not encode.

Outgoing *payloads* are bounded too: `send_data` classifies each payload against a
provisional per-call ceiling of `MAX_ADAPTER_SEND` (16 MiB) before copying it, and
rejects an oversized payload up front with the adapter-owned `OversizedSend` error
(surfaced as `Unknown`) rather than materializing a huge owned copy just to have
msquic reject it. The ceiling and the copy path are detailed in
[receive and send](./receive-and-send.md).

## What the adapter maps

Terminal precedence is **scoped**, not global:

- the first connection terminal reason wins for the connection;
- the first receive-side terminal reason wins for each stream;
- the first send-side terminal reason wins for each stream.

Because the scopes are independent, a receive-side reset does not suppress a
distinct peer `STOP_SENDING` on the send half of the same stream. Later
`ShutdownComplete` events perform cleanup but never overwrite an earlier terminal
reason in the same scope.

The msquic transport `error_code` that accompanies `ShutdownInitiatedByTransport`
is a QUIC transport code, not an HTTP/3 application code: it is retained for
diagnostics but never placed into `ApplicationClose`.

| MsQuic source | h3 result |
| --- | --- |
| `ConnectionEvent::ShutdownInitiatedByPeer { error_code }` | `ApplicationClose { error_code }` |
| Transport status `QUIC_STATUS_CONNECTION_TIMEOUT` / `QUIC_STATUS_CONNECTION_IDLE` | `Timeout` |
| Other `ShutdownInitiatedByTransport { status, error_code }` | `Undefined(MsQuicTransportError { status, error_code })` |
| Local connection close | `Undefined` with an adapter-owned local-close error (no invented peer code) |
| Connection channel closes without a recorded reason | `InternalError("connection closed without a terminal reason")` |
| `StreamEvent::PeerSendAborted { error_code }` | Receive side: `StreamTerminated { error_code }` |
| `StreamEvent::PeerReceiveAborted { error_code }` | Send side: `StreamTerminated { error_code }` |
| Stream shutdown caused by connection shutdown | `ConnectionErrorIncoming { connection_error }` |
| Failed `StartComplete` not caused by connection shutdown | `Unknown(status)` |
| Cancelled `SendComplete` with a recorded peer/connection reason | The recorded `StreamTerminated` or connection error |
| Cancelled `SendComplete` without a reason | `Unknown(QUIC_STATUS_ABORTED)` |
| `Receive` with data then FIN | Deliver data, then `Ok(None)` on the next poll |
| `Receive` with FIN and no data, or `PeerSendShutdown` | `Ok(None)` |
| `Receive` with no data and no FIN | No terminal event |
| `SendShutdownComplete { graceful: true }` | `poll_finish -> Ok(())` |
| `SendShutdownComplete { graceful: false }` | Recorded stream/connection error, otherwise `Unknown` |

For comparison, the reference adapters preserve the same boundaries: `h3-quinn`
maps peer application close to `ApplicationClose`, timeout to `Timeout`, other
connection failures to `Undefined`, and `Reset` / `Stopped` to `StreamTerminated`;
`s2n-quic-h3` maps `StreamReset` to `StreamTerminated` and a stream's
`ConnectionError` to `ConnectionErrorIncoming`. Both implement `reset` by sending
the supplied application code and report a `send_data`-while-busy misuse as an
error rather than panicking.

## Adapter-owned terminal vocabulary

Callback-facing terminal reasons live in three small, adapter-owned, scoped enums
in `../msquic-h3/src/error.rs`. Keeping them separate from the h3 types means the
callback path records *facts* and the polling path mints *h3 errors* exactly once,
at the boundary.

- **`ConnectionTerminal`**: `PeerApplication(u64)`, `Timeout`,
  `Transport { status, error_code }`, `LocalClose`, `Internal(&'static str)`.
  Only `LocalClose` is *provisional* (`is_provisional`).
- **`ReceiveTerminal`**: `Fin`, `Reset(u64)`, `Connection(ConnectionTerminal)`,
  `Internal(&'static str)`.
- **`SendTerminal`**: `Stopped(u64)`, `Connection`, `LocalReset(u64)`,
  `Failed(Status)`, `ProvisionalAbort`, `Internal`. Only `ProvisionalAbort` is
  provisional; `aborted_status()` is `QUIC_STATUS_ABORTED`.

The conversion helpers implement these exact terminal-to-h3 mappings:

| Adapter terminal | h3 result |
| --- | --- |
| `ConnectionTerminal::PeerApplication(code)` | `ApplicationClose { error_code: code }` |
| `ConnectionTerminal::Timeout` | `Timeout` |
| `ConnectionTerminal::Transport { status, error_code }` | `Undefined(Arc<MsQuicTransportError>)`, retaining both values |
| `ConnectionTerminal::LocalClose` | `Undefined(Arc<LocalConnectionClose>)` |
| `ConnectionTerminal::Internal(message)` | `InternalError(message)` |
| `ReceiveTerminal::Fin` | `Ok(None)` |
| `ReceiveTerminal::Reset(code)` | `StreamTerminated { error_code: code }` |
| `ReceiveTerminal::Connection(reason)` | `ConnectionErrorIncoming` via the connection conversion |
| `SendTerminal::Stopped(code)` | `StreamTerminated { error_code: code }` |
| `SendTerminal::Connection(reason)` | `ConnectionErrorIncoming` via the connection conversion |
| `ReceiveTerminal::Internal` / `SendTerminal::Internal` | `ConnectionErrorIncoming` containing `InternalError` |
| `SendTerminal::LocalReset(code)` | `Unknown(Box<LocalStreamReset>)` |
| `SendTerminal::Failed(status)` | `Unknown(Box::new(status))` |

`MsQuicTransportError`, `LocalConnectionClose`, and `LocalStreamReset` are small
`Debug + Display + Error + Send + Sync` types. The `Send + Sync` bounds are
mandatory: h3 stores these behind
`ConnectionErrorIncoming::Undefined(Arc<dyn Error + Send + Sync>)` and
`StreamErrorIncoming::Unknown(Box<dyn Error + Send + Sync>)`, so a type lacking
them could not be placed into those variants.

## Terminal-cause slots and commit-on-delivery

A terminal reason is produced by a msquic callback but consumed by a later poll,
so it is stored in a shared slot until the polling side reads it. Two slots exist
per relevant scope:

- The **connection slot** (`ConnTerminalState` in `../msquic-h3/src/terminal.rs`) holds
  the connection's terminal reason. `record` is a first-writer with
  provisional→specific refinement; `observe` freezes the slot so no later write
  can change what a reader already saw.
- The **send slot** (`SendTerminalSlot`, an `Arc<Mutex<Option<SendTerminal>>>`)
  holds the send half's reason; `publish_send` is the first-writer with
  provisional refinement.

The subtle part is *when* the connection slot is frozen. Freezing must not happen
while the slot is still empty, or a reader could latch an empty slot a moment
before the connection callback records the real cause. The adapter therefore uses
**commit-on-delivery**: it freezes the connection slot only at the point a cause
is actually delivered to h3, and only when a cause is genuinely present.

- `commit_conn` freezes the connection slot only when a cause is present at
  delivery — it never freezes an empty slot.
- `resolve_terminal` (used by the send half) is a non-freezing read of the
  reducer's terminal input that also encodes connection precedence; it decides
  *what* h3 will see without committing.
- `commit_send_winner` freezes the shared connection slot only when a `Connection`
  cause is the value actually being returned to h3.
- `observe_conn_winner` is an unconditional freeze, used only where the slot is
  guaranteed already populated (the receive `poll_data` connection arm).
- `peek_conn_terminal` is a plain non-freezing read.

`reset` reads its terminal via `resolve_terminal` but **never** commits, and a
graceful finish never commits either; only a cause actually surfaced to h3 freezes
the shared slot. On a connection-caused stream shutdown, the stream callback
records into the *shared* connection slot and publishes fallbacks to both halves,
so the receive and send observation points re-resolve the same shared winner.

### Terminal-cause refinement

Refinement is what lets a provisional reason be sharpened into a specific one
without ever regressing a specific reason back to a vaguer one:

- The connection slot starts as at most a provisional `LocalClose`. Only a
  provisional cause may be refined; once a specific cause
  (`PeerApplication`/`Timeout`/`Transport`) or an `Internal` cause is recorded, it
  is authoritative and never overwritten (`ConnTerminalState::record`,
  `is_provisional` in `error.rs`).
- The send-scope provisional `ProvisionalAbort` is *send-only*: it is never
  written into the connection slot, so a cancelled send that lacks a richer reason
  cannot mask a real connection cause. On the send side, any `Failed` status
  (including a real native `QUIC_STATUS_ABORTED`) is authoritative and never
  refined.

This is the rule that makes the "first reason wins per scope, but provisional may
be sharpened" guarantee precise.

## The send state machine

The send half is a pure reducer: `transition(state: &mut SendState, input:
SendInput) -> SendCommand` in `../msquic-h3/src/error.rs`. It takes no native
handle and produces exactly one command per input, which makes every transition
exhaustively table-testable without QUIC.

All order-sensitive send events ride **one** ordered channel so that
finish-versus-terminal ordering is unambiguous. The event vocabulary is
`SendEvent`: `Complete { cancelled }` (a `SendComplete` for a data send),
`TerminalWake` (a terminal reason became available), and `FinishComplete {
graceful }` (a `SendShutdownComplete`). `SendState` tracks the in-flight
bookkeeping (`send_submitting`, `send_inprogress`, `finish_submitting`,
`finish_started`, `finish_complete`, `reset_submitting`), and the reducer consults
`definitive` / `provisional_pending` helpers to apply the refinement rule above.
The reducer never freezes the shared connection slot — freezing is deferred to the
frontend commit-on-delivery points described earlier.

### Send-side transitions

The frontend executor loops in `../msquic-h3/src/stream.rs` translate the reducer's
commands into native submissions and h3 results:

- **`poll_ready`** resolves readiness, honoring the non-consuming finish guard so a
  poll after a finish has started does not consume the queued channel.
- **`send_data`** classifies the payload, copies it into an owned buffer, and
  submits it (see [receive and send](./receive-and-send.md) for the buffer
  ownership rules).
- **`poll_finish`** submits a graceful FIN and completes on the matching
  `FinishComplete { graceful: true }`.
- **`reset`** submits an `ABORT_SEND` with the clamped code and resolves its
  terminal via `resolve_terminal` without committing the connection slot.

A cancelled `SendComplete` that carries no richer reason surfaces
`Unknown(QUIC_STATUS_ABORTED)`; if a peer or connection reason was recorded, that
recorded reason is surfaced instead — the cancellation never masks it.

## Related

- The receive-side terminal transitions and the send buffer ownership are in
  [receive and send](./receive-and-send.md).
- How these slots and reducers stay memory-safe when a callback panics is in
  [callback safety](./callback-safety.md).
