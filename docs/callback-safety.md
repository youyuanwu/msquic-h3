# Callback safety

msquic invokes the adapter's callbacks from C, on msquic-owned threads, often
while holding internal locks. A Rust panic that unwound across that FFI boundary
would be undefined behavior, and a callback that mishandled handle ownership could
double-close a native object. This document describes the two disciplines that
keep the boundary sound: panic containment and take-ownership-only-on-success.

## Panic containment

Every one of the three callbacks (connection, stream, listener) runs its body
inside `guard_callback` (`../msquic-h3/src/callback.rs`), which wraps the work in
`catch_unwind(AssertUnwindSafe(...))`. Three outcomes are handled distinctly:

- **Normal return** — the body's result (including a legitimate `Err`, such as the
  receive path returning `QUIC_STATUS_PENDING`) passes through unchanged.
- **Already poisoned** — if a prior invocation on the same context poisoned it,
  the guard returns a caller-precomputed, event-aware result without running the
  body again.
- **Caught panic** — the guard runs a `recover` action and returns a safe status.

A contained panic force-closes the connection or stream toward the peer with the
HTTP/3 application error code `H3_INTERNAL_ERROR` (`0x0102`), and returns
`QUIC_STATUS_INTERNAL_ERROR` to msquic as the callback status where one is
required. When the `tracing` feature is enabled, `report_contained_panic` logs it.
The crate is built to abort rather than unwind, so `guard_callback` is a
defense-in-depth backstop that converts a would-be process abort at the FFI
boundary into an orderly connection/stream teardown.

### Poison, disposition, and recovery

Each context carries a poison flag: `ConnCtxSender` and `StreamSendCtx` use a plain
`bool` (they are entered by one thread at a time), while `ListenerCtxSender` uses
an `AtomicBool` because msquic can invoke listener callbacks **in parallel**
through a shared `&` context.

The result returned for a poisoned or panicking callback is **event-aware**,
because the right answer depends on what the event was:

- A teardown event (`ShutdownComplete` / `StopComplete`) returns `Ok` — there is
  nothing left to fail.
- An ownership-bearing or in-progress event (e.g. `NewConnection`,
  `PeerStreamStarted`) returns `INTERNAL_ERROR`.

The `recover` actions restore a safe state. `connection_recover` and
`stream_recover` force-close the handle through the injectable `ShutdownSeam`,
record an internal terminal reason, wake every waiter so no poller hangs, and
poison the context. `listener_recover` differs: it does not force-close or record
a terminal reason — it wakes the pending `accept()` and `shutdown()` waiters,
poisons the context, and returns an ownership-aware result (`Ok` if the new
connection was already owned, since its drop closed it, otherwise `Err` so msquic
performs the single reject). The `ShutdownSeam` used by the connection/stream paths
has three impls: `ConnectionRef` shuts the connection down
(`ConnectionShutdownFlags::NONE`), `StreamRef` aborts the stream
(`StreamShutdownFlags::ABORT`), and `NoShutdown` is a no-op bound to the listener
path (which rejects rather than force-closing).

## Ownership discipline at the callback boundary

Some callbacks receive a *borrowed* native handle (a `StreamRef` / `ConnectionRef`
that dereferences to the real type but never closes on drop) and may promote it to
an *owned* handle via `from_raw`. The rule is: **take ownership only after the
operation that could reject the handle has succeeded**, and once ownership is
taken, never close-then-return-`Err` (that would ask msquic to reject a handle it
no longer owns and trips a native assert).

Ownership within a single callback invocation is tracked by an **invocation-local**
`Cell<bool>` (created fresh per callback), not a shared flag — critical for the
listener, whose callbacks run in parallel:

- **Connection `PeerStreamStarted`**: validate the borrowed `StreamRef` first
  (query and check the stream id); only on success `Stream::from_raw` and set the
  ownership cell. If delivery to the acceptor then fails, drop the now-owned
  `H3Stream` and return `Ok` — dropping performs the native teardown, so returning
  `Err` as well would double-close. `connection_recover` returns `Ok` if ownership
  was taken, `Err` otherwise.
- **Listener `NewConnection`**: set the configuration on the borrowed reference
  first; if that fails, return the `Status` so msquic performs the single close.
  On success, `from_raw` and set the ownership cell; if delivery to the acceptor
  then fails, drop the owned `Connection` and return `Ok`. `listener_recover`
  mirrors this.

### Native stream teardown on drop

The native `msquic::Stream` lives behind an `Arc` shared by the send and receive
halves; dropping the last owner runs the binding's `Drop`, which calls
`StreamClose` without an explicit prior shutdown. This is memory-safe and
leak-free for send-buffer reclamation, and the argument rests on the native
library's behavior rather than on an adapter unit test — because the public API
cannot deterministically hold a real native send outstanding across a
drop-triggered `StreamClose`.

When a started-but-unshutdown stream is closed, msquic shuts it down abortively
with error code `0`. Crucially, msquic runs callbacks **inline** during close (it
blocks until pending work drains and still dispatches while the handle is closing),
so any outstanding send receives its cancelled `SendComplete` — and thus its
`Box<SendBuffer>` is reclaimed exactly once — before the binding drops the callback
context. No explicit shutdown-before-close is required for memory safety or leak
freedom. This inline-drain-on-close behavior is uniform across the `msquic`
binding's handle types (`Connection`, `Listener`, `Stream` all close the same way),
so it is a documented compatibility expectation on the linked library, not an
adapter-specific assumption.

What the adapter *does* assert with a test is its own exactly-once
`Box<SendBuffer>` reclamation on the production `SendComplete` path, via the
`CountingExec` seam (see [testing](./testing.md)). The one residual is a
protocol-quality concern, not a safety gate: a plain drop of a still-active stream
aborts it toward the peer with code `0` rather than a meaningful HTTP/3 code, and
would turn an in-flight graceful finish into an abort. Because h3 normally finishes
or `reset`s a stream before dropping it, this only affects abnormal cancellation;
emitting an explicit abort code before close is a reasonable future enhancement,
intentionally out of scope.

## Related

- The terminal reasons `recover` records and how they reach h3 are in the
  [error model](./error-model.md).
- The `ShutdownSeam` and the executor seams that make these paths testable are in
  [testing](./testing.md).
