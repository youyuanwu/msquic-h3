// Native-provenance features `native-find` / `native-src` are mutually exclusive
// (see Cargo.toml). Selecting BOTH is rejected by the upstream `msquic` build
// script (`feature src and find are mutually exclusive`), which runs before this
// crate compiles. Selecting NEITHER is a SUPPORTED type-check-only configuration:
// the crate compiles without linking a native library (this is exactly what the
// default-features CI `cargo check`/`clippy` job exercises), so no crate-level
// guard is imposed. A real build/link that resolves msquic symbols requires
// exactly one provenance. docs.rs builds under `native-src` via
// `[package.metadata.docs.rs]` in Cargo.toml.

use std::{
    cell::Cell,
    ffi::c_void,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    ready,
};
use h3::quic::{
    BidiStream, ConnectionErrorIncoming, OpenStreams, RecvStream, SendStream, StreamErrorIncoming,
};
use msquic::{
    BufferRef, Configuration, ConnectionEvent, ConnectionRef, ConnectionShutdownFlags,
    ReceiveFlags, SendFlags, Status, StatusCode, StreamEvent, StreamOpenFlags, StreamRef,
    StreamShutdownFlags, StreamStartFlags,
};

mod buffer;
use buffer::{SendBuffer, SendLen, classify_send_len, copy_into_send_buffer};
mod callback;
pub(crate) use callback::{
    CbClass, ForceShutdown, NoShutdown, PoisonFlag, ShutdownSeam, guard_callback,
    report_contained_panic,
};
mod terminal;
#[cfg(test)]
pub(crate) use terminal::new_conn_terminal_slot;
pub(crate) use terminal::{
    ConnTerminalSlot, ConnTerminalState, SendTerminalSlot, classify_conn_shutdown,
    classify_transport, commit_conn, load_winner, new_send_terminal_slot, observe_conn_winner,
    peek_conn_terminal, publish_send, record_conn_terminal,
};
mod error;
use error::{
    ConnectionTerminal, ReceiveTerminal, SendCommand, SendEvent, SendInput, SendPayload, SendPoll,
    SendState, SendTerminal, clamp_application_code, convert_conn, convert_recv, convert_send,
    convert_send_op, transition,
};
pub use error::{LocalConnectionClose, LocalStreamReset, MsQuicTransportError, OversizedSend};
mod listener;
pub use listener::Listener;
mod registration;
use registration::RundownGuard;
pub use registration::{Registration, WaitIdle};

/// Runtime native-library attestation (MF-3): the `native_version_preflight`
/// gate lives here. Test-only — it queries the live msquic handle for its
/// version + git hash and digests the loaded `libmsquic`.
#[cfg(test)]
mod attest;

/// Feature-gated public re-export of the ONE send-copy path so the separate
/// Criterion bench crate (`benches/send_copy.rs`) can reach it (MF-4). Only the
/// function is re-exported; `SendBuffer`'s name is never exposed. The surface
/// appears **only** when the committed `bench-internals` feature is enabled, so
/// no bench-only API leaks into the default library build.
#[cfg(feature = "bench-internals")]
pub mod bench_support {
    pub use crate::buffer::copy_into_send_buffer;
}

/// re-export msquic type
pub mod msquic {
    pub use ::msquic::*;
}

/// Acquire a mutex, recovering the guard if a panic on another thread poisoned
/// the lock.
///
/// FFI callbacks and rundown/waiter paths must never unwind across the msquic
/// boundary, so a poisoned lock is recovered via [`PoisonError::into_inner`]
/// rather than propagated as a panic. Every lock these paths touch guards only
/// plain data with no torn invariant, so the inner value is always safe to use.
pub(crate) fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// FFI callback panic containment (SF-E / FR-007).
//
// The upstream msquic `extern "C"` trampolines contain no `catch_unwind`, so a
// panic raised inside one of this crate's adapter callback bodies would unwind
// across the C boundary and abort the process. [`guard_callback`] is a
// defense-in-depth backstop that wraps each adapter body in
// `catch_unwind(AssertUnwindSafe(..))` and, on a caught panic, runs a
// class-specific `recover` action that force-closes the affected native handle,
// wakes the affected terminal waiters (msquic ignores the callback return status
// for many events, so a returned `Err` alone cannot be relied upon), and marks
// the ctx poisoned so any subsequent teardown event short-circuits. It contains
// only panics raised inside the crate's own closure bodies; a panic raised
// inside the upstream trampoline itself (before this frame is entered) is out of
// scope per FR-007.
// ---------------------------------------------------------------------------

/// The HTTP/3 `H3_INTERNAL_ERROR` application error code, conveyed to the peer
/// as the abort cause on a panic-contained connection/stream force-close.
pub(crate) const H3_INTERNAL_ERROR: u64 = 0x0102;

/// Build the internal-error status returned to msquic on a contained panic.
pub(crate) fn internal_error_status() -> Status {
    Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)
}

/// Owns the raw msquic connection handle together with its [`RundownGuard`].
///
/// Field order is load-bearing: `inner` is declared before `_guard`, so on drop
/// `ConnectionClose` (which releases the registration rundown ref) runs before
/// the guard decrements and wakes `wait_idle` waiters. The handle is shared
/// behind an `Arc`, so this drop only happens once the last `Arc<ConnHandle>`
/// (including `StreamOpener` clones) is gone.
#[derive(Debug)]
pub(crate) struct ConnHandle {
    inner: msquic::Connection,
    /// Shared connection terminal-reason slot. Cloned from the connection
    /// context so the stream opener (`OpenStreams::close`) can record a local
    /// close through the borrowed handle.
    terminal: ConnTerminalSlot,
    _guard: RundownGuard,
}

impl ConnHandle {
    pub(crate) fn new(
        inner: msquic::Connection,
        guard: RundownGuard,
        terminal: ConnTerminalSlot,
    ) -> Self {
        Self {
            inner,
            terminal,
            _guard: guard,
        }
    }

    /// The shared connection terminal-reason slot.
    pub(crate) fn terminal(&self) -> &ConnTerminalSlot {
        &self.terminal
    }
}

impl std::ops::Deref for ConnHandle {
    type Target = msquic::Connection;
    fn deref(&self) -> &msquic::Connection {
        &self.inner
    }
}

#[derive(Debug)]
pub struct Connection {
    conn: Arc<ConnHandle>,
    ctx: ConnCtxReceiver,
    opener: StreamOpener,
}

/// from callback send to fount end.
#[derive(Debug)]
struct ConnCtxSender {
    connected: Option<oneshot::Sender<Result<(), Status>>>,
    shutdown: Option<oneshot::Sender<()>>,
    bidi: Option<mpsc::UnboundedSender<Option<crate::H3Stream>>>,
    uni: Option<mpsc::UnboundedSender<Option<crate::H3Stream>>>,
    /// Shared connection terminal-reason slot (writer side).
    terminal: ConnTerminalSlot,
    /// Set by [`connection_recover`] after a contained callback panic (SF-E), so
    /// [`guard_callback`] short-circuits every subsequent event on this ctx.
    poisoned: bool,
    /// Connection-scoped test seam that forces an accepted-stream ID resolution
    /// to fail exactly once. Shares its atomic with
    /// [`ConnCtxReceiver::accepted_id_failpoint`] (created once in
    /// [`conn_ctx_channel`]) so a live [`Connection`] can arm it before an
    /// accept; lives in the shared connection state rather than a
    /// thread-local/process-global because msquic callbacks run on worker
    /// threads and a connection can migrate between them.
    #[cfg(test)]
    accepted_id_failpoint: Arc<AcceptedIdFailpoint>,
}

/// front end receive.
#[derive(Debug)]
struct ConnCtxReceiver {
    connected: Option<oneshot::Receiver<Result<(), Status>>>,
    shutdown: Option<oneshot::Receiver<()>>,
    bidi: mpsc::UnboundedReceiver<Option<crate::H3Stream>>,
    uni: mpsc::UnboundedReceiver<Option<crate::H3Stream>>,
    /// Shared connection terminal-reason slot (reader side).
    terminal: ConnTerminalSlot,
    /// Reader/frontend handle to the connection-scoped accepted-ID failpoint.
    /// Shares the same atomic as [`ConnCtxSender::accepted_id_failpoint`], so a
    /// test holding a live [`Connection`] (which owns this receiver) can arm the
    /// failpoint *before* a peer stream is accepted; the callback's
    /// `PeerStreamStarted` path then reads it through the sender-side clone.
    #[cfg(test)]
    accepted_id_failpoint: Arc<AcceptedIdFailpoint>,
}

fn conn_ctx_channel() -> (ConnCtxSender, ConnCtxReceiver) {
    let (conn_tx, conn_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (bidi_tx, bidi_rx) = mpsc::unbounded();
    let (uni_tx, uni_rx) = mpsc::unbounded();
    let terminal: ConnTerminalSlot = Arc::new(Mutex::new(ConnTerminalState::default()));
    // The accepted-ID failpoint atomic is shared (cloned) between the callback's
    // sender and the frontend's receiver so a live `Connection` can arm it.
    #[cfg(test)]
    let accepted_id_failpoint: Arc<AcceptedIdFailpoint> = Arc::new(AcceptedIdFailpoint::default());
    (
        ConnCtxSender {
            connected: Some(conn_tx),
            shutdown: Some(shutdown_tx),
            bidi: Some(bidi_tx),
            uni: Some(uni_tx),
            terminal: terminal.clone(),
            poisoned: false,
            #[cfg(test)]
            accepted_id_failpoint: accepted_id_failpoint.clone(),
        },
        ConnCtxReceiver {
            connected: Some(conn_rx),
            shutdown: Some(shutdown_rx),
            bidi: bidi_rx,
            uni: uni_rx,
            terminal,
            #[cfg(test)]
            accepted_id_failpoint,
        },
    )
}

/// Pure, unit-testable ID validation. MsQuic stream IDs are 62-bit so this
/// never fails for a live peer, but the [`h3::quic::InvalidStreamId`] branch is
/// still handled explicitly (a test seam and future callers can drive it).
fn validate_stream_id(raw: u64) -> Result<h3::quic::StreamId, h3::quic::InvalidStreamId> {
    h3::quic::StreamId::try_from(raw)
}

/// Wake both accept frontends by dropping the incoming-stream senders.
///
/// Used only on the fail-fast accepted-stream attachment path: once the adapter
/// has declared its own connection state invalid (an `Internal` terminal), it
/// must stop handing new streams to h3 and unpark any parked acceptor so it
/// observes the published terminal.
fn wake_acceptors(ctx: &mut ConnCtxSender) {
    ctx.uni.take();
    ctx.bidi.take();
}

/// Resolve and validate a peer-accepted stream's h3 [`h3::quic::StreamId`]
/// *before* native ownership is taken.
///
/// `query` produces the raw 62-bit ID from the still-borrowed `StreamRef`. On
/// any failure this publishes an `Internal` connection terminal, wakes both
/// acceptors, and returns the `Status` the callback must return so msquic closes
/// the rejected stream itself — the adapter never takes Rust ownership, so it
/// never double-closes. On success the caller is cleared to take ownership.
fn accept_stream_id(
    ctx: &mut ConnCtxSender,
    query: impl FnOnce() -> Result<u64, Status>,
) -> Result<h3::quic::StreamId, Status> {
    let raw = match query() {
        Ok(raw) => raw,
        Err(status) => {
            record_conn_terminal(
                &ctx.terminal,
                ConnectionTerminal::Internal("accepted stream ID query failed"),
            );
            wake_acceptors(ctx);
            return Err(status);
        }
    };
    match validate_stream_id(raw) {
        Ok(id) => Ok(id),
        Err(_) => {
            record_conn_terminal(
                &ctx.terminal,
                ConnectionTerminal::Internal("accepted stream ID is invalid"),
            );
            wake_acceptors(ctx);
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR))
        }
    }
}

/// Connection-scoped test seam forcing exactly one accepted-stream ID
/// resolution to fail. An atomic (not a thread-local) because the msquic
/// callback runs on a worker thread; consuming itself atomically identifies the
/// single rejected stream and stays correct under the parallel test harness.
#[cfg(test)]
#[derive(Debug, Default)]
struct AcceptedIdFailpoint {
    mode: std::sync::atomic::AtomicU8,
}

#[cfg(test)]
impl AcceptedIdFailpoint {
    const OFF: u8 = 0;
    const QUERY_FAIL: u8 = 1;
    const INVALID_ID: u8 = 2;

    /// Arm the next accepted-stream attachment to fail its native ID query.
    fn arm_query_fail(&self) {
        self.mode
            .store(Self::QUERY_FAIL, std::sync::atomic::Ordering::Relaxed);
    }

    /// Arm the next accepted-stream attachment to yield an invalid h3 ID.
    fn arm_invalid_id(&self) {
        self.mode
            .store(Self::INVALID_ID, std::sync::atomic::Ordering::Relaxed);
    }

    /// Consume the armed mode atomically and transform the queried raw ID:
    /// off → `Ok(raw)`, query-fail → `Err(status)`, invalid → an out-of-range
    /// ID that fails [`validate_stream_id`].
    fn maybe_override(&self, raw: u64) -> Result<u64, Status> {
        match self
            .mode
            .swap(Self::OFF, std::sync::atomic::Ordering::Relaxed)
        {
            Self::QUERY_FAIL => Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            Self::INVALID_ID => Ok(u64::MAX),
            _ => Ok(raw),
        }
    }
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(skip(ctx), level = "trace", ret, err)
)]
fn connection_callback(
    ctx: &mut ConnCtxSender,
    ev: msquic::ConnectionEvent,
    stream_owned: &Cell<bool>,
) -> Result<(), Status> {
    match ev {
        ConnectionEvent::Connected { .. } => {
            // A duplicate `Connected` (slot already taken) or a dropped receiver
            // (send `Err`) is a safe no-op; never panic across the FFI boundary.
            if let Some(tx) = ctx.connected.take() {
                let _ = tx.send(Ok(()));
            }
        }
        ConnectionEvent::ShutdownInitiatedByPeer { error_code } => {
            // Peer application close: record the exact HTTP/3 code. A peer close
            // has no lossless `Status`, so a pending connect waiter resolves as
            // `QUIC_STATUS_ABORTED` (the exact code stays in the terminal slot).
            record_conn_terminal(
                &ctx.terminal,
                ConnectionTerminal::PeerApplication(error_code),
            );
            if let Some(tx) = ctx.connected.take() {
                let _ = tx.send(Err(Status::new(StatusCode::QUIC_STATUS_ABORTED)));
            }
        }
        ConnectionEvent::ShutdownInitiatedByTransport { status, error_code } => {
            // Transport shutdown: timeout statuses become `Timeout`, everything
            // else retains the status + wire code. Resolve a pending connect
            // waiter with the real transport `Status` (not a synthetic ABORTED).
            record_conn_terminal(
                &ctx.terminal,
                classify_transport(status.clone(), error_code),
            );
            if let Some(tx) = ctx.connected.take() {
                let _ = tx.send(Err(status));
            }
        }
        ConnectionEvent::PeerStreamStarted { stream, flags } => {
            // Resolve and validate the stream ID against the still-BORROWED
            // `StreamRef` (auto-deref to `Stream::get_stream_id`). No owning
            // `Stream` is created until this succeeds, so a validation failure
            // returns the `Status` for msquic to close the rejected stream —
            // the adapter never took ownership, avoiding the reject-and-close
            // double-close hazard in native `stream_set.c`.
            #[cfg(test)]
            let failpoint = ctx.accepted_id_failpoint.clone();
            let query = || {
                let raw = stream.get_stream_id()?;
                #[cfg(test)]
                let raw = failpoint.maybe_override(raw)?;
                Ok(raw)
            };
            // Pre-ownership failure path: `accept_stream_id` already published the
            // internal terminal and woke the acceptors, so `?` returns the status
            // and msquic closes the rejected stream itself (single close). On
            // success we take native ownership only NOW.
            let id = accept_stream_id(ctx, query)?;
            // Success: only NOW take native ownership and attach. Mark the
            // per-invocation ownership token the instant ownership transfers, so a
            // subsequent panic (e.g. inside `H3Stream::attach`) tells
            // `connection_recover` the owned stream's Drop already closed it — and
            // recover must NOT return an Err that would make msquic re-close it
            // (the `stream_set.c` reject-and-close double-close hazard, F-C).
            let owned = unsafe { msquic::Stream::from_raw(stream.as_raw()) };
            stream_owned.set(true);
            let h3 = crate::H3Stream::attach(owned, id, ctx.terminal.clone());
            let target = if flags.contains(StreamOpenFlags::UNIDIRECTIONAL) {
                ctx.uni.as_ref()
            } else {
                ctx.bidi.as_ref()
            };
            if let Some(tx) = target {
                // Post-ownership delivery failure: the returned `SendError`
                // carries the owned `H3Stream`, which drops here and runs a
                // single `StreamClose`. Do NOT return `Err` — combined with the
                // close it would trip native `stream_set.c`'s reject assert.
                let _ = tx.unbounded_send(Some(h3));
            }
            // No target sender (fail-fast already dropped them): `h3` drops here
            // and closes exactly once.
        }
        ConnectionEvent::ShutdownComplete { .. } => {
            // A bare shutdown with no more-specific reason published is a local
            // close (provisional): `record` keeps any specific cause already
            // recorded by a peer/transport arm. `ShutdownComplete` itself carries
            // no error code.
            record_conn_terminal(&ctx.terminal, ConnectionTerminal::LocalClose);
            // If the connect waiter is still pending here (neither `Connected`
            // nor a `ShutdownInitiated*` arm resolved it — e.g. a bare local
            // close), resolve it as a failure so `connect()` returns `Err`
            // deterministically rather than mapping a `Canceled` drop.
            if let Some(tx) = ctx.connected.take() {
                let _ = tx.send(Err(Status::new(StatusCode::QUIC_STATUS_ABORTED)));
            }
            // clear all channels.
            ctx.uni.take();
            ctx.bidi.take();
            if let Some(shutdown) = ctx.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
        _ => {}
    }
    Ok(())
}

impl PoisonFlag for ConnCtxSender {
    fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

/// Event-aware poison disposition for the connection callback (SF-E). Once the
/// ctx is poisoned, a `PeerStreamStarted` (ownership-bearing) or any non-teardown
/// event is rejected with `INTERNAL_ERROR` so msquic does not expect the adapter
/// to have taken ownership; a `ShutdownComplete` (teardown) is a safe `Ok(())`
/// no-op (the handle is already force-closed and its waiters already woken).
fn conn_poison_disp(ev: &ConnectionEvent) -> Result<(), Status> {
    match ev {
        ConnectionEvent::ShutdownComplete { .. } => Ok(()),
        _ => Err(internal_error_status()),
    }
}

/// Panic-recovery action for the connection callback (SF-E). Runs only after a
/// caught panic, once the body's `&mut ctx` borrow has ended. Force-closes the
/// connection (no `ABORT` flag exists; the abort is conveyed by `code`), records
/// an internal connection terminal, wakes every connection waiter (connect
/// one-shot, both accept frontends, the shutdown waiter) so nothing hangs, and
/// marks the ctx poisoned.
///
/// RETURN VALUE is ownership-aware for the `PeerStreamStarted` double-close
/// hazard (F-C). If a peer stream's native ownership was already taken this
/// invocation (`stream_owned == true`, i.e. the panic happened AFTER
/// `Stream::from_raw`, e.g. inside `H3Stream::attach`), the owned stream's Drop
/// already performed the single `StreamClose`; returning `Ok(())` prevents msquic
/// from also closing that stream (`core/stream_set.c` reject → `core/stream.c`
/// double-close assert). Otherwise — a pre-ownership `PeerStreamStarted` failure
/// or ANY non-ownership-bearing connection event (`stream_owned` stays false) —
/// return `Err(INTERNAL_ERROR)` so msquic performs the sole rejection/close.
fn connection_recover(
    ctx: &mut ConnCtxSender,
    seam: &dyn ShutdownSeam,
    stream_owned: &Cell<bool>,
) -> Result<(), Status> {
    report_contained_panic(CbClass::Connection);
    seam.force_shutdown(
        ForceShutdown::Connection(ConnectionShutdownFlags::NONE),
        H3_INTERNAL_ERROR,
    );
    record_conn_terminal(
        &ctx.terminal,
        ConnectionTerminal::Internal("connection callback panicked"),
    );
    if let Some(tx) = ctx.connected.take() {
        let _ = tx.send(Err(internal_error_status()));
    }
    // Drop both accept-frontend senders so a parked acceptor observes the
    // published internal terminal instead of hanging.
    wake_acceptors(ctx);
    if let Some(shutdown) = ctx.shutdown.take() {
        let _ = shutdown.send(());
    }
    ctx.poisoned = true;
    if stream_owned.get() {
        // Owned peer stream already closed by its Drop; do NOT let msquic
        // re-close it. The connection is still force-closed above.
        Ok(())
    } else {
        Err(internal_error_status())
    }
}

impl Connection {
    ///
    /// The rundown count is reserved synchronously when this is called (before
    /// the returned future is polled), so even a queued/unpolled connect is
    /// tracked by [`Registration::wait_idle`]. The registration must outlive all
    /// its connections; `wait_idle` then `drop(reg)` is the safe teardown order.
    pub fn connect<'a>(
        reg: &'a Registration,
        config: &'a Configuration,
        server_name: &'a str,
        server_port: u16,
    ) -> impl std::future::Future<Output = Result<Self, Status>> + 'a {
        // Reserved now, before the caller ever polls. If the future is dropped
        // without completing, the guard drops and decrements.
        let guard = RundownGuard::new(reg.state().clone());
        async move {
            let (mut ctx, mut crx) = conn_ctx_channel();
            let handler = move |conn_ref: ConnectionRef, ev: ConnectionEvent| {
                let disp = conn_poison_disp(&ev);
                // Per-invocation peer-stream ownership token (F-C): a fresh `Cell`
                // per callback so parallel connection callbacks never share it.
                // Set true right after `PeerStreamStarted`'s `from_raw`; read by
                // `connection_recover` to stay double-close-safe.
                let stream_owned = Cell::new(false);
                guard_callback(
                    &mut ctx,
                    &conn_ref,
                    disp,
                    |c| connection_callback(c, ev, &stream_owned),
                    |c, seam| connection_recover(c, seam, &stream_owned),
                )
            };
            // Build the ordered handle immediately after `open`, before `start`
            // and before the first await, so every success/error/cancellation
            // path uses ConnHandle's proven ConnectionClose-then-guard drop
            // order rather than the async future's unspecified capture layout.
            let inner = msquic::Connection::open(reg.raw(), handler)?;
            let conn = Arc::new(ConnHandle::new(inner, guard, crx.terminal.clone()));
            conn.start(config, server_name, server_port)?;
            // wait for connection. The one-shot carries `Result<(), Status>`: a
            // transport/peer shutdown before `Connected` resolves it with the
            // real cause, so the caller sees the actual `Status` (handshake
            // failure, timeout, refusal, ...) instead of a synthetic ABORTED,
            // and never hangs.
            crx.connected
                .take()
                .ok_or_else(|| Status::new(StatusCode::QUIC_STATUS_ABORTED))?
                .await
                .map_err(|_| Status::new(StatusCode::QUIC_STATUS_ABORTED))??;

            let opener = StreamOpener::new(conn.clone());

            Ok(Self {
                conn,
                ctx: crx,
                opener,
            })
        }
    }

    /// attach to an accepted connection
    pub(crate) fn attach(inner: msquic::Connection, guard: RundownGuard) -> Self {
        let (mut ctx, crx) = conn_ctx_channel();
        let handler = move |conn_ref: ConnectionRef, ev: ConnectionEvent| {
            let disp = conn_poison_disp(&ev);
            // Per-invocation peer-stream ownership token (F-C); see `connect`.
            let stream_owned = Cell::new(false);
            guard_callback(
                &mut ctx,
                &conn_ref,
                disp,
                |c| connection_callback(c, ev, &stream_owned),
                |c, seam| connection_recover(c, seam, &stream_owned),
            )
        };
        inner.set_callback_handler(handler);
        let conn = Arc::new(ConnHandle::new(inner, guard, crx.terminal.clone()));

        let opener = StreamOpener::new(conn.clone());

        Self {
            conn,
            ctx: crx,
            opener,
        }
    }

    /// Returns the connection shutdown waiter.
    ///
    /// The first call waits for shutdown completion. Later calls return a
    /// waiter that resolves immediately.
    pub fn get_shutdown_waiter(&mut self) -> ConnectionShutdownWaiter {
        // If the waiter was already taken, hand back an immediately-resolving
        // one (its sender is dropped) rather than panicking. `wait` treats a
        // dropped sender as a benign completion.
        let rx = self.ctx.shutdown.take().unwrap_or_else(|| {
            let (_tx, rx) = oneshot::channel();
            rx
        });
        ConnectionShutdownWaiter { rx }
    }
}

/// Test-only seam to arm the connection-scoped accepted-stream-ID failpoint from
/// a *live* `Connection`. Because the atomic is stored in the shared connection
/// state (cloned into both the callback's sender and this frontend's receiver),
/// a Phase 8 loopback test holding the accepted `Connection` can arm the
/// failpoint *before* the peer opens a stream; the real `PeerStreamStarted`
/// accept path then trips on it, driving the native reject + connection
/// `H3_INTERNAL_ERROR` close.
#[cfg(test)]
impl Connection {
    /// Arm the next accepted-stream attachment to fail its native ID query.
    pub(crate) fn arm_accepted_id_query_fail(&self) {
        self.ctx.accepted_id_failpoint.arm_query_fail();
    }

    /// Arm the next accepted-stream attachment to yield an invalid h3 ID.
    pub(crate) fn arm_accepted_id_invalid(&self) {
        self.ctx.accepted_id_failpoint.arm_invalid_id();
    }

    /// Test-only view of the shared failpoint atomic — the same one the callback
    /// consults on the accept path — so a test can prove arming took effect.
    pub(crate) fn accepted_id_failpoint(&self) -> &Arc<AcceptedIdFailpoint> {
        &self.ctx.accepted_id_failpoint
    }
}

/// Fail-fast terminal check for the accept frontends.
///
/// Returns the converted connection error only when an *internal* terminal has
/// been published, so an adapter-internal failure is reported ahead of any
/// streams still queued. Observing it also freezes the slot. A non-internal
/// terminal returns `None`, so the caller drains queued streams first.
fn fail_fast_terminal(slot: &ConnTerminalSlot) -> Option<ConnectionErrorIncoming> {
    let mut g = lock_recover(slot);
    if g.has_internal() {
        return g.observe().map(convert_conn);
    }
    None
}

/// Freeze the terminal slot and convert the recorded reason for h3, committing
/// on delivery only.
///
/// Routes through [`commit_conn`], so this is a **committing** accept poll *only
/// when it actually delivers a cause*: a recorded reason is frozen and converted
/// exactly as before; an **empty** slot returns a defined internal error
/// **without** freezing (`observed` stays unset), leaving the slot refinable so a
/// later real cause can still be recorded and delivered (Fix 1 — FR-002 /
/// FR-003). This makes the accept path uniform with the send/open/data delivery
/// points, which all freeze only on genuine delivery.
fn observe_terminal(slot: &ConnTerminalSlot) -> ConnectionErrorIncoming {
    match commit_conn(slot) {
        Some(t) => convert_conn(t),
        None => ConnectionErrorIncoming::InternalError(
            "connection closed without a terminal reason".to_string(),
        ),
    }
}

/// wait for connection to be fully shutdown.
pub struct ConnectionShutdownWaiter {
    rx: oneshot::Receiver<()>,
}
impl ConnectionShutdownWaiter {
    /// wait for connection to be fully shutdown.
    pub async fn wait(self) {
        // A dropped sender (connection torn down without a clean
        // `ShutdownComplete`) resolves the wait rather than panicking.
        let _ = self.rx.await;
    }
}

/// responsible for open streams on a connection.
#[derive(Debug)]
pub struct StreamOpener {
    conn: Arc<ConnHandle>,
    bidi_temp: Option<OpeningStream>,
    uni_temp: Option<OpeningStream>,
    /// Connection-scoped open seam (prod = [`StreamOpenExecutor`]); lets a test
    /// substitute the native open/start without a live connection.
    open_exec: Box<dyn OpenExec>,
}

impl Clone for StreamOpener {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            bidi_temp: None,
            uni_temp: None,
            // An in-flight OpeningStream is not shared, so a clone starts with
            // empty temps and a fresh production open seam.
            open_exec: Box::new(StreamOpenExecutor),
        }
    }
}

/// Server accept streams
impl<B: Buf> h3::quic::Connection<B> for Connection {
    type RecvStream = H3RecvStream;

    type OpenStreams = StreamOpener;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_accept_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        // Fail-fast: an internal terminal is reported immediately, ahead of any
        // streams still queued in the channel (the adapter has declared its own
        // connection state invalid). Normal peer/transport/local shutdown keeps
        // the drain-then-terminal ordering below.
        if let Some(err) = fail_fast_terminal(&self.ctx.terminal) {
            return std::task::Poll::Ready(Err(err));
        }
        match ready!(self.ctx.uni.poll_next_unpin(cx)) {
            // wrap for h3 type. Drop the send stream part.
            Some(Some(s)) => std::task::Poll::Ready(Ok(s.recv)),
            // Channel drained/closed: report the recorded terminal reason.
            Some(None) | None => std::task::Poll::Ready(Err(observe_terminal(&self.ctx.terminal))),
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_accept_bidi(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        if let Some(err) = fail_fast_terminal(&self.ctx.terminal) {
            return std::task::Poll::Ready(Err(err));
        }
        match ready!(self.ctx.bidi.poll_next_unpin(cx)) {
            // wrap for h3 type
            Some(Some(s)) => std::task::Poll::Ready(Ok(s)),
            Some(None) | None => std::task::Poll::Ready(Err(observe_terminal(&self.ctx.terminal))),
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn opener(&self) -> Self::OpenStreams {
        StreamOpener::new(self.conn.clone())
    }
}

/// Create new streams from connection.
impl<B: Buf> OpenStreams<B> for StreamOpener {
    type BidiStream = H3Stream;

    type SendStream = H3SendStream;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_open_bidi(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        Self::poll_open_inner(
            &self.conn,
            self.open_exec.as_ref(),
            false,
            &mut self.bidi_temp,
            cx,
        )
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_open_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let res = ready!(Self::poll_open_inner(
            &self.conn,
            self.open_exec.as_ref(),
            true,
            &mut self.uni_temp,
            cx
        ));
        // get the send part.
        std::task::Poll::Ready(res.map(|s| s.send))
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn close(&mut self, code: h3::error::Code, _reason: &[u8]) {
        // An application-initiated shutdown may proceed straight to
        // `ShutdownComplete`, so record the provisional local-close reason
        // before the downcall. A concurrent peer/transport cause may still
        // refine it until an accept frontend observes the terminal.
        record_conn_terminal(self.conn.terminal(), ConnectionTerminal::LocalClose);
        self.open_exec
            .submit_conn_shutdown(&self.conn, clamp_application_code(code.value()));
    }
}

impl StreamOpener {
    fn new(conn: Arc<ConnHandle>) -> Self {
        Self {
            conn,
            bidi_temp: None,
            uni_temp: None,
            open_exec: Box::new(StreamOpenExecutor),
        }
    }

    /// Open a native stream, then drive it to a fully-identified [`H3Stream`].
    ///
    /// Never panics: a connection-caused cancellation of the pending start maps
    /// to a connection error, and a start cancelled with no published reason to
    /// a nested internal error. The local stream ID is sourced from the
    /// `StartComplete` outcome and validated before an `H3Stream` is built.
    fn poll_open_inner(
        conn: &Arc<ConnHandle>,
        open_exec: &dyn OpenExec,
        uni: bool,
        holder: &mut Option<OpeningStream>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<H3Stream, StreamErrorIncoming>> {
        use std::task::Poll;
        // 1. Fail fast if the connection already published a terminal. A delivery
        //    of the cause to h3 commits (freezes) it via `commit_conn` (SF-C).
        if let Some(reason) = commit_conn(conn.terminal()) {
            *holder = None; // drop any in-flight OpeningStream
            return Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_conn(reason),
            }));
        }
        // 2. Create + start the native stream if none is in flight.
        if holder.is_none() {
            match open_exec.submit_open_start(conn, uni) {
                Ok(opening) => *holder = Some(opening),
                Err(status) => {
                    // A shutdown may have raced the open; prefer the terminal and
                    // commit it on delivery (SF-C). A `None` (no cause) surfaces the
                    // raw status as `Unknown` and does NOT freeze the slot.
                    return Poll::Ready(Err(match commit_conn(conn.terminal()) {
                        Some(reason) => StreamErrorIncoming::ConnectionErrorIncoming {
                            connection_error: convert_conn(reason),
                        },
                        None => StreamErrorIncoming::Unknown(status.into()),
                    }));
                }
            }
        }
        // 3. Await StartComplete on the OpeningStream's `start` receiver.
        let raw = {
            let Some(opening) = holder.as_mut() else {
                return Poll::Pending;
            };
            match Pin::new(&mut opening.start).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(raw)) => raw, // Result<u64, Status> from StartComplete
                Poll::Ready(Err(oneshot::Canceled)) => {
                    // Sender dropped by ShutdownComplete without a StartComplete:
                    // a connection-caused cancellation (never a panic).
                    *holder = None;
                    return Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                        connection_error: stream_open_conn_error(conn.terminal()),
                    }));
                }
            }
        };
        // 4. StartComplete carried a status; classify it and finalize.
        let Some(opening) = holder.take() else {
            return Poll::Pending;
        };
        Poll::Ready(match classify_start_outcome(raw, conn.terminal()) {
            Ok(id) => Ok(opening.finalize(id)),
            Err(e) => Err(e),
        })
    }
}

/// Connection error for a stream-open whose start channel was cancelled.
///
/// A published connection terminal is the true cause; a cancellation with no
/// recorded reason is a defined internal error, never a synthetic peer code.
/// Delivering the cause to h3 commits (freezes) it via [`commit_conn`] (SF-C);
/// the empty-slot internal-error fallback does not freeze.
fn stream_open_conn_error(slot: &ConnTerminalSlot) -> ConnectionErrorIncoming {
    match commit_conn(slot) {
        Some(reason) => convert_conn(reason),
        None => ConnectionErrorIncoming::InternalError(
            "stream start cancelled without a terminal reason".to_string(),
        ),
    }
}

/// Classify a `StartComplete` outcome into a validated local [`h3::quic::StreamId`]
/// or a stream error.
///
/// A failed start prefers a published connection terminal, else surfaces the raw
/// `Status` as `Unknown`. A delivered connection cause commits (freezes) via
/// [`commit_conn`] (SF-C); the `Unknown` fallback does not freeze. A successful
/// start validates the ID; an out-of-range ID is an adapter-internal fault
/// (never `Unknown`).
fn classify_start_outcome(
    raw: Result<u64, Status>,
    slot: &ConnTerminalSlot,
) -> Result<h3::quic::StreamId, StreamErrorIncoming> {
    match raw {
        Err(status) => Err(match commit_conn(slot) {
            Some(reason) => StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_conn(reason),
            },
            None => StreamErrorIncoming::Unknown(status.into()),
        }),
        Ok(raw_id) => {
            validate_stream_id(raw_id).map_err(|_| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(
                    "local stream ID is invalid".to_string(),
                ),
            })
        }
    }
}

/// bypass for StreamOpener
impl<B: Buf> OpenStreams<B> for Connection {
    type BidiStream = H3Stream;

    type SendStream = H3SendStream;

    fn poll_open_bidi(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        OpenStreams::<B>::poll_open_bidi(&mut self.opener, cx)
    }

    fn poll_open_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        OpenStreams::<B>::poll_open_send(&mut self.opener, cx)
    }

    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        OpenStreams::<B>::close(&mut self.opener, code, reason)
    }
}

/// Msquic Stream.
#[derive(Debug)]
pub struct H3Stream {
    send: H3SendStream,
    recv: H3RecvStream,
}
#[derive(Debug)]
pub struct H3SendStream {
    /// Every native send-side verb goes through `exec`; the production
    /// [`StreamExecutor`] (held here) owns the `Arc<msquic::Stream>` that keeps
    /// the send half's native handle alive. This ownership is independent of the
    /// recv half (which holds its own `Arc` clone), and it lets a test inject a
    /// double without a live connection.
    exec: Box<dyn SendExec>,
    sctx: SendStreamReceiveCtx,
}
#[derive(Debug)]
pub struct H3RecvStream {
    exec: Box<dyn RecvExec>,
    rctx: RecvStreamReceiveCtx,
}

/// Send-side command-executor seam: acts on an already-open stream. Stored behind
/// a trait object in [`H3SendStream`] so a test double (the Phase 8 `CountingExec`)
/// can replace it without a live connection. The production implementation is
/// [`StreamExecutor`].
///
/// `Send + Sync` keep `H3SendStream` `Send + Sync` (as it was when it held the
/// `Arc<msquic::Stream>` directly).
trait SendExec: std::fmt::Debug + Send + Sync {
    /// Box the already-validated non-empty [`SendBuffer`], `Box::into_raw` it as
    /// `client_context`, and call `Stream::send`. On an immediate `Err` the box is
    /// reconstructed and dropped here (MsQuic took no ownership and will deliver no
    /// `SendComplete`), before returning.
    fn submit_send(&mut self, buf: SendBuffer) -> Result<(), Status>;
    /// `Stream::shutdown(GRACEFUL)` — the FIN for a graceful finish.
    fn submit_graceful(&mut self) -> Result<(), Status>;
    /// `Stream::shutdown(ABORT_SEND)` with an already-clamped application code.
    /// Wired at the `SendStream::reset` call site (Phase 7).
    fn submit_reset(&mut self, code: u64) -> Result<(), Status>;
}

/// Production send-side seam. Owns the `Arc<msquic::Stream>` and issues real
/// native send/shutdown downcalls.
#[derive(Debug)]
struct StreamExecutor {
    stream: Arc<msquic::Stream>,
}

/// Shared owned-buffer submission transaction (Phase 6 exactly-once contract).
///
/// This is the ONE place that performs the `Box::into_raw` → hand-off →
/// immediate-`Err` `Box::from_raw` ownership dance for a send. It boxes the
/// already-validated non-empty [`SendBuffer`], leaks it as the `client_context`
/// raw pointer, and invokes `native` with the buffer slice and that pointer. On
/// an immediate `Err` (MsQuic took no ownership and will deliver no
/// `SendComplete`) the box is reconstructed and dropped here — the sole
/// reclamation — before the error is returned; on `Ok` the box stays outstanding
/// until the `SendComplete` callback reclaims it.
///
/// Both the production [`StreamExecutor`] and the test `CountingExec` route
/// through this helper, so a regression in the ownership handoff cannot pass in
/// one and fail in the other.
fn submit_owned_send(
    buf: SendBuffer,
    native: impl FnOnce(&[BufferRef], *const c_void) -> Result<(), Status>,
) -> Result<(), Status> {
    // Box the self-referential SendBuffer and hand its raw pointer to `native`
    // as `client_context`. `buf`'s single `BufferRef` points into the owned
    // `Bytes` heap storage and stays valid because the box is not moved again
    // until the `SendComplete` callback (or the immediate-failure reclaim below)
    // reconstructs it.
    let cc: *mut SendBuffer = Box::into_raw(Box::new(buf));
    // SAFETY: `cc` is a live `Box<SendBuffer>` leaked just above; `(*cc)` is a
    // unique, valid reference. Its `buffers` memory stays valid until the
    // `SendComplete` callback reconstructs and drops the box (`Stream::send`'s
    // documented contract) or the immediate-failure reclaim below runs.
    let res = unsafe { native((*cc).buffers(), cc as *const c_void) };
    if let Err(status) = res {
        // Immediate failure: MsQuic took no ownership and delivers no
        // `SendComplete`, so reclaim the box here (mirroring the callback drop)
        // before returning. Reclaimed exactly once.
        // SAFETY: `cc` is the pointer from `Box::into_raw` above; MsQuic did not
        // take ownership on an error, so this is the sole reclamation.
        drop(unsafe { Box::from_raw(cc) });
        return Err(status);
    }
    Ok(())
}

impl SendExec for StreamExecutor {
    fn submit_send(&mut self, buf: SendBuffer) -> Result<(), Status> {
        // The single owned-buffer ownership transaction (shared with the test
        // seam). FIN is not set on a data send — a graceful finish is a separate
        // `submit_graceful` shutdown call.
        submit_owned_send(buf, |buffers, cc| {
            // SAFETY: `buffers` borrows the boxed `SendBuffer`'s stable heap
            // storage (valid for the whole `Stream::send` call), and `cc` is the
            // matching `client_context` MsQuic returns once in `SendComplete`.
            unsafe { self.stream.send(buffers, SendFlags::NONE, cc) }
        })
    }

    fn submit_graceful(&mut self) -> Result<(), Status> {
        // GRACEFUL FIN; the error_code is ignored for a graceful shutdown.
        self.stream.shutdown(StreamShutdownFlags::GRACEFUL, 0)
    }

    fn submit_reset(&mut self, code: u64) -> Result<(), Status> {
        self.stream.shutdown(StreamShutdownFlags::ABORT_SEND, code)
    }
}

/// Receive-side command-executor seam: the recv half's only native downcall is
/// `stop_sending` (`Stream::shutdown(ABORT_RECEIVE)`). Stored behind a trait
/// object in [`H3RecvStream`] so a test double can assert the *actual* clamped
/// code submitted with no live stream. Production is [`RecvStreamExecutor`],
/// which also owns the recv half's `Arc<msquic::Stream>` clone (SF-1), keeping
/// the native handle alive for as long as the recv half exists.
trait RecvExec: std::fmt::Debug + Send + Sync {
    /// `Stream::shutdown(ABORT_RECEIVE)` with an already-clamped application code.
    fn submit_stop_sending(&self, code: u64) -> Result<(), Status>;
    /// Complete a previously pended receive indication on THIS stream (SF-A),
    /// re-arming its receive callbacks and advancing its flow-control window.
    /// `len` is always the FULL saved pended-indication length (never a
    /// drained/partial length). Behind the seam so the per-stream
    /// pending→complete cycle is deterministically testable. The default is a
    /// no-op for doubles that never exercise the backpressure cycle.
    fn submit_receive_complete(&self, _len: u64) {}
}

/// Production receive-side seam. Owns the `Arc<msquic::Stream>` and issues the
/// real native `ABORT_RECEIVE` shutdown.
#[derive(Debug)]
struct RecvStreamExecutor {
    stream: Arc<msquic::Stream>,
}

impl RecvExec for RecvStreamExecutor {
    fn submit_stop_sending(&self, code: u64) -> Result<(), Status> {
        self.stream
            .shutdown(StreamShutdownFlags::ABORT_RECEIVE, code)
    }

    fn submit_receive_complete(&self, len: u64) {
        // Per-stream, already-bound, thread-safe. msquic tolerates an inline or
        // concurrent completion issued before/while the RECEIVE callback returns
        // (lock-free active-call flag), so this is safe from the drain path.
        self.stream.receive_complete(len);
    }
}

/// Connection-scoped open seam: *creates and starts* the native stream, returning
/// the pre-ID [`OpeningStream`]. Separate from [`SendExec`] because it cannot act
/// on an `Arc<Stream>` that does not exist yet. Stored in [`StreamOpener`] so a
/// test can substitute the native open/start. Production is [`StreamOpenExecutor`].
trait OpenExec: std::fmt::Debug + Send + Sync {
    fn submit_open_start(&self, conn: &ConnHandle, uni: bool) -> Result<OpeningStream, Status>;
    /// `Connection::shutdown(NONE)` for `OpenStreams::close`, with an
    /// already-clamped application code. Behind the seam so a test double can
    /// assert the *actual* submitted value without a live connection.
    fn submit_conn_shutdown(&self, conn: &ConnHandle, code: u64);
}

/// Production open seam: the real `Stream::open` + `Stream::start`.
#[derive(Debug)]
struct StreamOpenExecutor;

impl OpenExec for StreamOpenExecutor {
    fn submit_open_start(&self, conn: &ConnHandle, uni: bool) -> Result<OpeningStream, Status> {
        H3Stream::open_and_start(conn, uni)
    }
    fn submit_conn_shutdown(&self, conn: &ConnHandle, code: u64) {
        conn.shutdown(ConnectionShutdownFlags::NONE, code);
    }
}

#[cfg(test)]
impl H3SendStream {
    /// Test/internal constructor: inject any [`SendExec`] (including the seam-test
    /// double) plus a send-side context whose channel senders the test retains.
    /// No live connection or native stream is required — the send path touches
    /// only `exec` and `sctx`.
    fn with_exec(exec: Box<dyn SendExec>, sctx: SendStreamReceiveCtx) -> Self {
        H3SendStream { exec, sctx }
    }
}

#[cfg(test)]
impl H3RecvStream {
    /// Test constructor: inject any [`RecvExec`] (the clamp-recording double) plus
    /// a receive-side context. No live stream is required — `stop_sending` touches
    /// only `exec` and `rctx`.
    fn with_exec(exec: Box<dyn RecvExec>, rctx: RecvStreamReceiveCtx) -> Self {
        H3RecvStream { exec, rctx }
    }
}

#[cfg(test)]
impl StreamOpener {
    /// Test constructor: inject any [`OpenExec`] (the clamp-recording double) so
    /// `close` can be exercised against a real (unstarted) `ConnHandle` without
    /// any native shutdown reaching the wire.
    fn with_open_exec(conn: Arc<ConnHandle>, open_exec: Box<dyn OpenExec>) -> Self {
        Self {
            conn,
            bidi_temp: None,
            uni_temp: None,
            open_exec,
        }
    }
}

/// Per-STREAM receive backpressure bound (SF-A / SC-004): the maximum number of
/// undrained, adapter-buffered received bytes tolerated on a single stream
/// before its receive callback pends (returns `QUIC_STATUS_PENDING`) instead of
/// completing the indication. Because a msquic PENDING pauses receive callbacks
/// for THAT stream only and `receive_complete` re-arms THAT stream, the budget
/// is tracked per stream, not connection-wide; a connection's adapter receive
/// memory is therefore bounded by `(MAX_RECV_BUFFER + one in-flight indication)
/// × the negotiated max concurrent streams`.
const MAX_RECV_BUFFER: usize = 1024 * 1024; // 1 MiB per stream

/// The single `Mutex`-guarded receive accounting for ONE stream. The buffered
/// byte counter, the pending flag, and the saved pending-indication length are
/// all mutated under one lock, so the callback's (increment → decide-pend →
/// publish) and the drain's (decrement → decide-complete) transitions can never
/// interleave to underflow the counter or lose a just-published pend.
#[derive(Debug, Default)]
struct RecvState {
    /// Undrained bytes currently sitting in this stream's receive channel.
    buffered: usize,
    /// True once this stream returned `QUIC_STATUS_PENDING` and has not yet been
    /// re-armed via `receive_complete`.
    pend_outstanding: bool,
    /// FULL length of the indication that was pended — never a drained or
    /// partial length. Replayed verbatim to `receive_complete` when the drain
    /// frees the budget.
    pending_indication_len: u64,
    /// Peak `buffered` ever observed on this stream (test/measurement
    /// observation for the SC-004 bound; must stay ≤ `MAX_RECV_BUFFER` + one
    /// in-flight indication).
    peak: usize,
}

/// Per-stream receive budget, shared (as an `Arc`) between the stream callback
/// (writer) and the `poll_data` drain (reader). Each local or accepted stream
/// owns its own independent 1 MiB budget; there is no connection-wide meter. See
/// [`RecvState`].
#[derive(Debug, Default)]
struct RecvBudget {
    state: Mutex<RecvState>,
}

/// Outcome of the callback-side [`RecvBudget::admit`] transition.
enum Admit {
    /// Bytes published and the stream is within budget: complete normally.
    Accepted,
    /// Bytes published and the stream is now at/over budget: the caller must
    /// return `QUIC_STATUS_PENDING` (the indication is NOT completed yet).
    Pend,
    /// Publication failed (the frontend receive half was dropped). The
    /// `RecvState` mutation was rolled back under the same lock, so accounting is
    /// exactly as if the indication never arrived; the caller drops the sender
    /// and issues no pend.
    PublishFailed,
}

impl RecvBudget {
    /// Lock the receive state, recovering a poisoned lock rather than panicking
    /// (FFI callbacks must never unwind across the msquic boundary).
    fn lock(&self) -> MutexGuard<'_, RecvState> {
        lock_recover(&self.state)
    }

    /// Callback-side single synchronized transition — **accounting BEFORE
    /// drain-visibility, publish and decision atomic under one lock**. Adds a
    /// full indication of `total` bytes, decides whether the stream must pend
    /// (saving this indication's FULL length), and then — still holding the
    /// lock, as the LAST step — runs `publish` to make the bytes drain-visible.
    /// `publish` returns `true` on a successful hand-off (or when there is
    /// nothing to publish). On a failed hand-off (dropped frontend) the entire
    /// mutation is **rolled back under the same lock** so a concurrent drain can
    /// never observe stale accounting, and [`Admit::PublishFailed`] is returned.
    /// Because the publish is the last step, a concurrent drain can never
    /// subtract before the increment lands.
    fn admit(&self, total: usize, publish: impl FnOnce() -> bool) -> Admit {
        let mut st = self.lock();
        // Snapshot for exact rollback if the publication fails.
        let prev_buffered = st.buffered;
        let prev_peak = st.peak;
        let prev_pend = st.pend_outstanding;
        let prev_len = st.pending_indication_len;

        st.buffered += total;
        if st.buffered > st.peak {
            st.peak = st.buffered;
        }
        let pend = st.buffered >= MAX_RECV_BUFFER;
        if pend {
            st.pend_outstanding = true;
            st.pending_indication_len = total as u64;
        }

        if publish() {
            if pend { Admit::Pend } else { Admit::Accepted }
        } else {
            // Roll the whole transition back: buffered, peak watermark, and the
            // pend fields all return to their pre-indication values.
            st.buffered = prev_buffered;
            st.peak = prev_peak;
            st.pend_outstanding = prev_pend;
            st.pending_indication_len = prev_len;
            Admit::PublishFailed
        }
    }

    /// Drain-side single synchronized transition. Subtracts `len` drained bytes
    /// and, if a pend is outstanding and the budget has fallen below the bound,
    /// clears the pend and returns the FULL saved indication length to hand to
    /// `receive_complete` (re-arming the stream). Returns `None` otherwise.
    fn on_drained(&self, len: usize) -> Option<u64> {
        let mut st = self.lock();
        // The publish-last ordering in `admit` guarantees the increment for these
        // bytes was committed before they became drain-visible, so this can never
        // underflow. Catch a regression in debug/test builds rather than let
        // `saturating_sub` silently clamp it.
        debug_assert!(
            st.buffered >= len,
            "recv buffered underflow: buffered={} drained={len}",
            st.buffered
        );
        st.buffered = st.buffered.saturating_sub(len);
        if st.pend_outstanding && st.buffered < MAX_RECV_BUFFER {
            st.pend_outstanding = false;
            let complete = st.pending_indication_len;
            st.pending_indication_len = 0;
            Some(complete)
        } else {
            None
        }
    }
}

#[cfg(test)]
impl RecvBudget {
    fn buffered(&self) -> usize {
        self.lock().buffered
    }
    fn peak(&self) -> usize {
        self.lock().peak
    }
    fn pend_outstanding(&self) -> bool {
        self.lock().pend_outstanding
    }
    fn pending_indication_len(&self) -> u64 {
        self.lock().pending_indication_len
    }
}

/// Explicit receive-side event delivered from the stream callback to the
/// frontend receive half.
///
/// Splitting the receive path into explicit events makes a graceful FIN, a peer
/// reset, an empty non-FIN notification, and a connection failure observably
/// distinct at `poll_data` (they were previously all collapsed into a bare
/// channel close). Data from a single notification is always enqueued before any
/// terminal marker from that same notification.
enum ReceiveEvent {
    /// Normal received bytes.
    Data(Bytes),
    /// Peer finished sending gracefully (FIN / `PeerSendShutdown`): clean EOF.
    Fin,
    /// Peer reset the receive side (`PeerSendAborted`) with this code.
    Reset(u64),
    /// The whole connection terminated with this reason.
    Connection(ConnectionTerminal),
    /// A STREAM-LOCAL internal adapter failure (e.g. a contained callback panic
    /// on this stream, SF-E). It maps DIRECTLY to a stream-scoped
    /// [`ReceiveTerminal::Internal`] and must NOT touch the shared connection
    /// terminal slot: unlike [`ReceiveEvent::Connection`], it never freezes the
    /// connection winner, so a later genuine peer/transport connection cause is
    /// still observed by the accept/open frontends (F-A / MF-1 / SF-C).
    Internal(&'static str),
}

struct StreamSendCtx {
    start: Option<oneshot::Sender<Result<u64, Status>>>,
    /// Single ordered send-event channel (MF-1): data `SendComplete`, terminal
    /// wakes, and finish completion all ride this one FIFO so chronological
    /// finish-vs-terminal order is preserved by construction.
    send: Option<mpsc::UnboundedSender<SendEvent>>,
    /// Shared sticky send-terminal slot; the callback publishes peer
    /// `STOP_SENDING` and connection-shutdown reasons here (first-writer).
    send_terminal: SendTerminalSlot,
    /// Shared connection terminal slot (FR-013). On a CONNECTION-caused stream
    /// shutdown the callback publishes its derived fallback here (respecting
    /// first-writer / refinement) rather than into a detached per-stream copy, so
    /// a later refinement of the shared slot reaches the stream's observation
    /// points.
    conn_terminal: ConnTerminalSlot,
    receive: Option<mpsc::UnboundedSender<ReceiveEvent>>,
    /// Per-stream receive backpressure budget (SF-A). The callback (writer)
    /// commits its accounting and enqueue under this budget's single lock; the
    /// matching `Arc` in the receive half's ctx drains it.
    recv_budget: Arc<RecvBudget>,
    /// First-writer-wins guard for the receive scope: once a `Fin`, `Reset`, or
    /// `Connection` terminal has been published, later receive events are
    /// suppressed (e.g. the usual `Receive { FIN }` then `PeerSendShutdown`
    /// sequence must not enqueue a duplicate FIN).
    receive_terminal_sent: bool,
    /// Set by [`stream_recover`] after a contained callback panic (SF-E), so
    /// [`guard_callback`] short-circuits every subsequent event on this ctx.
    poisoned: bool,
}

/// ctx for receiving data on frontend.
#[derive(Debug)]
struct RecvStreamReceiveCtx {
    /// Cached, validated stream identity. Resolved before an `H3RecvStream` is
    /// exposed (from `StartComplete` for local streams, from the borrowed
    /// `get_stream_id` query for accepted streams), so `recv_id` never queries a
    /// native parameter and cannot panic after shutdown.
    id: h3::quic::StreamId,
    receive: mpsc::UnboundedReceiver<ReceiveEvent>,
    /// Sticky receive terminal. Once a terminal event has been drained it is
    /// stored here and every later `poll_data` returns the same class without
    /// re-polling the channel.
    terminal: Option<ReceiveTerminal>,
    /// Shared connection terminal slot (FR-013). On a connection-caused receive
    /// terminal, `poll_data` resolves *and freezes* the shared winner here (the
    /// same freeze-on-observation the accept frontends use) before storing the
    /// sticky reason, so the receive half reports the refined connection cause.
    conn_terminal: ConnTerminalSlot,
    /// SF-6: local, sticky end-of-stream flag set by OUR OWN `stop_sending`.
    /// Distinct from `terminal` (a peer/connection-caused reason); when set,
    /// `poll_data` returns a clean `Ok(None)` and `terminal` is left untouched.
    receive_closed: bool,
    /// Per-stream receive backpressure budget (SF-A). The drain (reader)
    /// decrements it on each delivered `Data` and, when a pend has been freed,
    /// hands the FULL saved indication length back to `receive_complete`. Shares
    /// the same `Arc` the callback (writer) accounts against.
    recv_budget: Arc<RecvBudget>,
}

/// ctx for sending data on frontend.
#[derive(Debug)]
struct SendStreamReceiveCtx {
    /// Cached, validated stream identity (see [`RecvStreamReceiveCtx::id`]); read
    /// by `send_id` without a native query.
    id: h3::quic::StreamId,
    /// Shared sticky send-terminal slot (see [`SendTerminalSlot`]). Loaded on
    /// every reducer input as the current winner; local candidates are published
    /// here via the first-writer helper.
    send_terminal: SendTerminalSlot,
    /// Shared connection terminal slot (FR-013). When the send winner is a
    /// `Connection` terminal, a delivering `poll_ready` / `poll_finish` / `send_data`
    /// commits *and freezes* this shared slot (via
    /// [`SendStreamReceiveCtx::commit_send_winner`]) so the send half reports the
    /// refined connection cause consistently with the receive half and the accept
    /// frontends; the reducer-input read ([`SendStreamReceiveCtx::resolve_terminal`])
    /// is non-freezing.
    conn_terminal: ConnTerminalSlot,
    /// Single ordered send-event source (MF-1): data completion, terminal wake,
    /// and finish completion. The reducer observes it only as a [`SendPoll`].
    send: mpsc::UnboundedReceiver<SendEvent>,
    /// Pure send-side reducer bookkeeping (replaces the old `send_inprogress`).
    reducer: SendState,
}

/// Frontend *receiver* ends held by an [`OpeningStream`] until the stream ID is
/// known. The matching *sender* halves live in the callback-owned
/// [`StreamSendCtx`]; holding these keeps every channel open (so callback sends
/// never fail) and lets `finalize` move them into the [`H3Stream`] halves.
#[derive(Debug)]
struct PreIdReceivers {
    start: oneshot::Receiver<Result<u64, Status>>,
    send: mpsc::UnboundedReceiver<SendEvent>,
    /// Shared send-terminal slot clone, moved into the send half by `finalize`.
    send_terminal: SendTerminalSlot,
    /// Shared connection terminal slot clone, moved into BOTH halves by
    /// `finalize` so each observation point can resolve+freeze the same winner.
    conn_terminal: ConnTerminalSlot,
    receive: mpsc::UnboundedReceiver<ReceiveEvent>,
    /// Per-stream receive budget (SF-A), carried into the receive half's ctx.
    recv_budget: Arc<RecvBudget>,
}

/// A locally opened stream whose native handle is started but whose h3
/// [`h3::quic::StreamId`] is not yet known. Private to [`StreamOpener`]; the
/// only stream form allowed to exist without a cached ID. On a successful
/// `StartComplete` it is consumed into an [`H3Stream`] with concrete ID fields
/// in both halves.
#[derive(Debug)]
struct OpeningStream {
    stream: Arc<msquic::Stream>,
    /// Pending-start receiver, polled by `poll_open_inner` until `StartComplete`.
    start: oneshot::Receiver<Result<u64, Status>>,
    /// The remaining receiver ends, moved into the `H3Stream` halves by
    /// `finalize` once the ID is validated.
    tail: PreIdTail,
}

/// The non-start receiver ends of an [`OpeningStream`] (kept apart so `start` can
/// be polled independently while these wait for `finalize`).
#[derive(Debug)]
struct PreIdTail {
    send: mpsc::UnboundedReceiver<SendEvent>,
    send_terminal: SendTerminalSlot,
    /// Shared connection terminal slot clone, split into both halves' ctxs by
    /// `finalize`.
    conn_terminal: ConnTerminalSlot,
    receive: mpsc::UnboundedReceiver<ReceiveEvent>,
    /// Per-stream receive budget (SF-A), carried into the receive half's ctx.
    recv_budget: Arc<RecvBudget>,
}

impl OpeningStream {
    /// Consume into an [`H3Stream`] once `StartComplete` has yielded a validated
    /// ID. The `start` receiver has already been driven to completion and is
    /// dropped here; the remaining three receivers move into the two halves.
    fn finalize(self, id: h3::quic::StreamId) -> H3Stream {
        let OpeningStream {
            stream,
            start: _,
            tail,
        } = self;
        let PreIdTail {
            send,
            send_terminal,
            conn_terminal,
            receive,
            recv_budget,
        } = tail;
        H3Stream {
            send: H3SendStream {
                exec: Box::new(StreamExecutor {
                    stream: stream.clone(),
                }),
                sctx: SendStreamReceiveCtx {
                    id,
                    send_terminal,
                    conn_terminal: conn_terminal.clone(),
                    send,
                    reducer: SendState::new(),
                },
            },
            recv: H3RecvStream {
                exec: Box::new(RecvStreamExecutor { stream }),
                rctx: RecvStreamReceiveCtx {
                    id,
                    receive,
                    terminal: None,
                    conn_terminal,
                    receive_closed: false,
                    recv_budget,
                },
            },
        }
    }
}

/// Pre-ID variant of the stream channel builder: no [`h3::quic::StreamId`] is
/// required or produced. Builds the callback-owned [`StreamSendCtx`] (senders)
/// and returns the matching receiver ends bundled, so an [`OpeningStream`] can
/// hold them until `StartComplete` yields the ID.
///
/// `conn_terminal` is the connection's SHARED terminal slot (FR-013), cloned into
/// the callback ctx (the writer of the connection-caused fallback) and carried in
/// the receivers so both frontend halves resolve+freeze the same winner.
fn stream_ctx_channel_pre_id(conn_terminal: ConnTerminalSlot) -> (StreamSendCtx, PreIdReceivers) {
    let (start_tx, start_rx) = oneshot::channel::<Result<u64, Status>>();
    let (send_tx, send_rx) = mpsc::unbounded();
    let send_terminal = new_send_terminal_slot();
    let (receive_tx, receive_rx) = mpsc::unbounded();
    let recv_budget = Arc::new(RecvBudget::default());
    (
        StreamSendCtx {
            start: Some(start_tx),
            send: Some(send_tx),
            send_terminal: send_terminal.clone(),
            conn_terminal: conn_terminal.clone(),
            receive: Some(receive_tx),
            recv_budget: recv_budget.clone(),
            receive_terminal_sent: false,
            poisoned: false,
        },
        PreIdReceivers {
            start: start_rx,
            send: send_rx,
            send_terminal,
            conn_terminal,
            receive: receive_rx,
            recv_budget,
        },
    )
}

/// ID-bearing stream channel builder used where the identity is already known
/// (accepted streams and tests). Splits the pre-ID receivers into the two
/// frontend halves' ctxs directly. Creates a fresh, standalone connection
/// terminal slot; tests that need to drive connection-terminal refinement use
/// [`stream_ctx_channel_with_conn`] to inject a shared slot.
#[cfg(test)]
fn stream_ctx_channel(
    id: h3::quic::StreamId,
) -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    stream_ctx_channel_with_conn(id, new_conn_terminal_slot())
}

/// ID-bearing stream channel builder sharing an explicit connection terminal
/// slot with the caller, so a test can record/refine the connection terminal and
/// observe how the stream halves resolve+freeze it (FR-013).
#[cfg(test)]
fn stream_ctx_channel_with_conn(
    id: h3::quic::StreamId,
    conn_terminal: ConnTerminalSlot,
) -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    let (ctx, recv) = stream_ctx_channel_pre_id(conn_terminal);
    let PreIdReceivers {
        start: _,
        send,
        send_terminal,
        conn_terminal,
        receive,
        recv_budget,
    } = recv;
    (
        ctx,
        SendStreamReceiveCtx {
            id,
            send_terminal,
            conn_terminal: conn_terminal.clone(),
            send,
            reducer: SendState::new(),
        },
        RecvStreamReceiveCtx {
            id,
            receive,
            terminal: None,
            conn_terminal,
            receive_closed: false,
            recv_budget,
        },
    )
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(skip(ctx), level = "trace", ret)
)]
fn stream_callback(ctx: &mut StreamSendCtx, ev: StreamEvent) -> Result<(), Status> {
    match ev {
        StreamEvent::StartComplete { status, id, .. } => {
            // Duplicate `StartComplete` (slot already taken) or a dropped
            // `OpeningStream` receiver is a safe no-op. The `id: u62` outcome is
            // the local stream's identity source (validated by poll_open_inner).
            if let Some(tx) = ctx.start.take() {
                let result = if status.is_ok() { Ok(id) } else { Err(status) };
                let _ = tx.send(result);
            }
        }
        StreamEvent::SendComplete {
            cancelled,
            client_context,
        } => {
            // Owned-buffer reclamation (Phase 6): reconstruct and drop the
            // `Box<SendBuffer>` handed to MsQuic as `client_context` BEFORE
            // enqueueing the completion, so the allocation is reclaimed exactly
            // once even if the frontend receiver is gone. MsQuic guarantees
            // `SendComplete` ends its use of the submitted buffers.
            //
            // SAFETY: every successful adapter send passes exactly one
            // `Box::into_raw(Box<SendBuffer>)` as `client_context`, and MsQuic
            // returns that same pointer here exactly once.
            match std::ptr::NonNull::new(client_context as *mut SendBuffer) {
                Some(cc) => {
                    // Non-null: exactly-once reclaim, then forward the outcome flag
                    // to the reducer via the single ordered channel (MF-1). A gone
                    // frontend is a safe no-op.
                    drop(unsafe { Box::from_raw(cc.as_ptr()) });
                    if let Some(send) = ctx.send.as_ref() {
                        let _ = send.unbounded_send(SendEvent::Complete { cancelled });
                    }
                }
                None => {
                    // A null context breaks the ownership invariant (every
                    // successful send attaches one `Box<SendBuffer>`). Do NOT emit a
                    // normal `Complete`: publish an internal send terminal and wake
                    // the send half so `poll_ready` / `poll_finish` surface the
                    // internal error rather than a false readiness (see
                    // "Send-side transitions" in `docs/error-model.md`).
                    publish_send(
                        &ctx.send_terminal,
                        SendTerminal::Internal("SendComplete missing client context"),
                    );
                    if let Some(send) = ctx.send.as_ref() {
                        let _ = send.unbounded_send(SendEvent::TerminalWake);
                    }
                }
            }
        }
        StreamEvent::Receive {
            total_buffer_length,
            buffers,
            flags,
            ..
        } => {
            // Full-completion receive backpressure (SF-A). The FULL indication is
            // ALWAYS copied (no data is ever dropped); the accounting and the
            // pend decision are committed under the per-stream `RecvState` lock,
            // and the bytes are made drain-visible as the LAST step of that same
            // locked transition — so a concurrent drain can never subtract before
            // the increment lands. When the stream's buffered bytes reach
            // `MAX_RECV_BUFFER` the event returns `QUIC_STATUS_PENDING` (below) to
            // pause THIS stream without completing the indication yet.
            let mut pend = false;
            if !ctx.receive_terminal_sent && ctx.receive.is_some() {
                // Pre-sum the buffer lengths for a single right-sized allocation.
                // msquic reports the same total in `total_buffer_length`.
                let total: usize = buffers.iter().map(|br| br.as_bytes().len()).sum();
                debug_assert_eq!(total as u64, *total_buffer_length);
                let mut b = BytesMut::with_capacity(total);
                for br in buffers {
                    // skip empty buffs.
                    if !br.as_bytes().is_empty() {
                        b.put_slice(br.as_bytes());
                    }
                }
                let b = b.freeze();
                // ONE synchronized transition: increment → decide-pend (saving
                // this indication's FULL length) → publish under the lock. On a
                // failed publication the whole mutation is rolled back inside
                // `admit`, so a dropped frontend leaves accounting untouched.
                match ctx.recv_budget.admit(total, || {
                    // An empty, non-FIN notification produces NO event (Finding
                    // 8): a zero-length receive is not by itself end-of-stream,
                    // and counts as a successful (no-op) publication.
                    if b.is_empty() {
                        true
                    } else if let Some(receive) = ctx.receive.as_ref() {
                        receive.unbounded_send(ReceiveEvent::Data(b)).is_ok()
                    } else {
                        false
                    }
                }) {
                    Admit::Pend => pend = true,
                    Admit::Accepted => {}
                    Admit::PublishFailed => {
                        // Failed delivery (frontend `H3RecvStream` dropped): drop
                        // the sender so no further receive events are attempted,
                        // and issue NO pend — no receiver means no stuck window,
                        // and the stream is being torn down regardless. Accounting
                        // was already rolled back under the lock inside `admit`.
                        ctx.receive.take();
                    }
                }
            }
            // A FIN flag is a clean end-of-stream marker (peer finished sending).
            // Preserved even on an over-budget indication: the full indication
            // (including a FIN) was already copied and enqueued above.
            if flags.contains(ReceiveFlags::FIN) {
                publish_recv_terminal(ctx, ReceiveEvent::Fin);
            }
            // Over-budget: pause THIS stream by pending (the indication is NOT
            // completed until the drain frees the budget and calls
            // `receive_complete` with the FULL saved length).
            if pend {
                return Err(Status::new(StatusCode::QUIC_STATUS_PENDING));
            }
        }
        StreamEvent::PeerSendShutdown => {
            // Peer gracefully finished sending: clean end-of-stream.
            publish_recv_terminal(ctx, ReceiveEvent::Fin);
        }
        StreamEvent::PeerSendAborted { error_code } => {
            // Peer RESET_STREAM on its send half: surfaces at `poll_data` as
            // `StreamTerminated { error_code }`, distinct from a graceful FIN.
            publish_recv_terminal(ctx, ReceiveEvent::Reset(error_code));
        }
        StreamEvent::PeerReceiveAborted { error_code } => {
            // Peer STOP_SENDING on OUR send half: publish the sticky send terminal
            // and wake the send frontend through the single ordered channel so
            // `poll_ready`/`poll_finish` observe `Stopped(code)` even with no send
            // in flight (SC-003). First-writer / MF-2 refinement is in `publish_send`.
            publish_send(&ctx.send_terminal, SendTerminal::Stopped(error_code));
            if let Some(send) = ctx.send.as_ref() {
                let _ = send.unbounded_send(SendEvent::TerminalWake);
            }
        }
        StreamEvent::SendShutdownComplete { graceful } => {
            // Graceful finish (or abort) completed: an ordinary send event on the
            // single ordered channel (MF-1), so finish-vs-terminal order holds.
            if let Some(send) = ctx.send.as_ref() {
                let _ = send.unbounded_send(SendEvent::FinishComplete { graceful });
            }
        }
        StreamEvent::ShutdownComplete {
            connection_shutdown,
            connection_shutdown_by_app,
            connection_closed_remotely,
            connection_error_code,
            connection_close_status,
            ..
        } => {
            // A connection-caused stream shutdown surfaces the connection error
            // on the receive half and, symmetrically, publishes it as the sticky
            // send terminal so the send half observes the same reason. The derived
            // fallback is PUBLISHED into the SHARED connection slot (respecting
            // first-writer / refinement) rather than finalized as a detached
            // per-stream copy, so a later refinement of that shared slot (e.g. a
            // provisional `LocalClose` refined to a specific peer/transport cause)
            // still reaches this stream's observation points (FR-013). The reason
            // carried on the receive marker / send terminal is only a defensive
            // fallback: each observation point re-resolves the shared winner. A
            // stream-local shutdown publishes no terminal.
            if connection_shutdown {
                let reason = classify_conn_shutdown(
                    connection_shutdown_by_app,
                    connection_closed_remotely,
                    connection_error_code,
                    connection_close_status,
                );
                record_conn_terminal(&ctx.conn_terminal, reason.clone());
                publish_recv_terminal(ctx, ReceiveEvent::Connection(reason.clone()));
                publish_send(&ctx.send_terminal, SendTerminal::Connection(reason));
            }
            // close all channels
            ctx.receive.take();
            ctx.send.take();
            ctx.start.take();
        }
        _ => {}
    }
    Ok(())
}

impl PoisonFlag for StreamSendCtx {
    fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

/// Event-aware poison disposition for the stream callback (SF-E). Once the ctx
/// is poisoned, a `ShutdownComplete` (teardown) is a safe `Ok(())` no-op — the
/// handle is already force-closed and its waiters woken, and re-running teardown
/// could only double-complete the Phase 2 receive state — while any other event
/// is conservatively rejected with `INTERNAL_ERROR`.
fn stream_poison_disp(ev: &StreamEvent) -> Result<(), Status> {
    match ev {
        StreamEvent::ShutdownComplete { .. } => Ok(()),
        _ => Err(internal_error_status()),
    }
}

/// Panic-recovery action for the stream callback (SF-E). Runs only after a
/// caught panic, once the body's `&mut ctx` borrow has ended. Force-closes the
/// stream (`StreamShutdownFlags::ABORT`), publishes an internal send terminal and
/// an internal receive terminal and wakes both halves (plus any pending
/// `StartComplete` opener), so no send/receive/open waiter hangs; the `poisoned`
/// short-circuit then prevents a follow-up teardown callback from re-touching the
/// Phase 2 receive state. Marks the ctx poisoned and returns `INTERNAL_ERROR`.
fn stream_recover(ctx: &mut StreamSendCtx, seam: &dyn ShutdownSeam) -> Result<(), Status> {
    report_contained_panic(CbClass::Stream);
    seam.force_shutdown(
        ForceShutdown::Stream(StreamShutdownFlags::ABORT),
        H3_INTERNAL_ERROR,
    );
    // Wake the send half: publish an internal send terminal, wake it through the
    // single ordered channel, then close the channel.
    publish_send(
        &ctx.send_terminal,
        SendTerminal::Internal("stream callback panicked"),
    );
    if let Some(send) = ctx.send.as_ref() {
        let _ = send.unbounded_send(SendEvent::TerminalWake);
    }
    ctx.send.take();
    // Wake the receive half with a STREAM-LOCAL internal terminal
    // (first-writer-wins; a real terminal already published wins). This uses the
    // stream-scoped `ReceiveEvent::Internal` — NOT a `Connection` event — so it
    // never freezes the shared connection slot on the EMPTY value; a later
    // genuine peer/transport connection cause is still delivered to the frontends
    // (F-A / MF-1 / SF-C).
    publish_recv_terminal(ctx, ReceiveEvent::Internal("stream callback panicked"));
    // Resolve a still-pending `StartComplete` opener so `poll_open_inner` fails
    // fast instead of hanging.
    if let Some(start) = ctx.start.take() {
        let _ = start.send(Err(internal_error_status()));
    }
    ctx.poisoned = true;
    Err(internal_error_status())
}

/// Publish a receive-side terminal event under first-writer-wins.
///
/// The first `Fin`/`Reset`/`Connection` wins and suppresses later receive
/// events. After publishing, the receive sender is dropped so no further events
/// are delivered; bytes already enqueued are preserved by the channel and are
/// still observed before the terminal.
fn publish_recv_terminal(ctx: &mut StreamSendCtx, ev: ReceiveEvent) {
    if ctx.receive_terminal_sent {
        return;
    }
    if let Some(receive) = ctx.receive.as_ref() {
        let _ = receive.unbounded_send(ev);
    }
    ctx.receive_terminal_sent = true;
    ctx.receive.take();
}

impl H3Stream {
    /// Attach to a peer-accepted stream whose identity was already validated
    /// from the borrowed `StreamRef` (see [`accept_stream_id`]). Takes native
    /// ownership of `stream` (the caller must have confirmed success first).
    ///
    /// `conn_terminal` is the accepting connection's SHARED terminal slot, so an
    /// accepted stream reports the same refinable connection cause the accept
    /// frontends do (FR-013).
    pub(crate) fn attach(
        stream: msquic::Stream,
        id: h3::quic::StreamId,
        conn_terminal: ConnTerminalSlot,
    ) -> Self {
        let (mut ctx, recv) = stream_ctx_channel_pre_id(conn_terminal);
        let handler = move |stream_ref: StreamRef, ev: StreamEvent| {
            let disp = stream_poison_disp(&ev);
            guard_callback(
                &mut ctx,
                &stream_ref,
                disp,
                |c| stream_callback(c, ev),
                stream_recover,
            )
        };
        stream.set_callback_handler(handler);
        // The ID is known, so finalize straight away. An accepted stream never
        // receives `StartComplete`, so its `start` receiver simply drops.
        OpeningStream {
            stream: Arc::new(stream),
            start: recv.start,
            tail: PreIdTail {
                send: recv.send,
                send_terminal: recv.send_terminal,
                conn_terminal: recv.conn_terminal,
                receive: recv.receive,
                recv_budget: recv.recv_budget,
            },
        }
        .finalize(id)
    }

    /// Open + start a native stream, returning the pre-ID [`OpeningStream`]. The
    /// h3 identity is not known yet; it arrives later via `StartComplete` and is
    /// resolved by [`StreamOpener::poll_open_inner`].
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", err, ret)
    )]
    fn open_and_start(conn: &ConnHandle, uni: bool) -> Result<OpeningStream, Status> {
        let (mut ctx, recv) = stream_ctx_channel_pre_id(conn.terminal().clone());
        let handler = move |stream_ref: StreamRef, ev: StreamEvent| {
            let disp = stream_poison_disp(&ev);
            guard_callback(
                &mut ctx,
                &stream_ref,
                disp,
                |c| stream_callback(c, ev),
                stream_recover,
            )
        };

        let flag = match uni {
            true => StreamOpenFlags::UNIDIRECTIONAL,
            false => StreamOpenFlags::NONE,
        };

        // `conn` derefs to the native `msquic::Connection` for the open downcall.
        let s = msquic::Stream::open(conn, flag, handler)?;
        s.start(StreamStartFlags::NONE)?; // id arrives later via StartComplete
        Ok(OpeningStream {
            stream: Arc::new(s),
            start: recv.start,
            tail: PreIdTail {
                send: recv.send,
                send_terminal: recv.send_terminal,
                conn_terminal: recv.conn_terminal,
                receive: recv.receive,
                recv_budget: recv.recv_budget,
            },
        })
    }
}

impl<B: Buf> SendStream<B> for H3SendStream {
    // h3 calls `poll_ready` after `send_data` to await the in-flight send.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), StreamErrorIncoming>> {
        use std::task::Poll;
        // SF-2 non-consuming finish guard: once a finish has started, `poll_ready`
        // must NOT poll (and thereby consume) the shared send channel — a consumed
        // `FinishComplete` would be lost and hang a later `poll_finish`. Resolve
        // purely from non-consuming state: a published terminal winner is surfaced;
        // otherwise the finish-after-poll_ready misuse (non-sticky).
        if self.sctx.reducer.finish_started {
            return match self.sctx.resolve_terminal() {
                // Delivery point: commit (freeze) a connection cause before it is
                // returned to h3.
                Some(w) => Poll::Ready(Err(convert_send(self.sctx.commit_send_winner(w)))),
                None => Poll::Ready(Err(convert_send(SendTerminal::Internal(
                    "poll_ready after finish",
                )))),
            };
        }
        // First input polls the channel; subsequent inputs are fed results.
        let mut input = SendInput::PollReady {
            poll: self.poll_send_channel(cx),
            terminal: self.sctx.resolve_terminal(),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::Pending => return Poll::Pending, // waker already registered
                SendCommand::ReturnReady => return Poll::Ready(Ok(())),
                SendCommand::ReturnError(t) => {
                    // Delivery point: commit the connection slot on the way out.
                    return Poll::Ready(Err(convert_send(self.sctx.commit_send_winner(t))));
                }
                SendCommand::RepollReady => {
                    // Re-poll the channel in THIS call so a synchronously-queued
                    // paired terminal / channel close (the closure point) is drained
                    // or the waker re-armed — the retained provisional cancellation
                    // (MF-2) is never returned to the caller.
                    input = SendInput::PollReady {
                        poll: self.poll_send_channel(cx),
                        terminal: self.sctx.resolve_terminal(),
                    };
                }
                SendCommand::PublishTerminal {
                    candidate,
                    continuation,
                } => {
                    let winner = publish_send(&self.sctx.send_terminal, candidate);
                    input = SendInput::TerminalPublished {
                        winner,
                        continuation,
                    };
                }
                _ => {
                    return Poll::Ready(Err(convert_send(SendTerminal::Internal(
                        "unexpected poll_ready command",
                    ))));
                }
            }
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret, err)
    )]
    fn send_data<T: Into<h3::quic::WriteBuf<B>>>(
        &mut self,
        data: T,
    ) -> Result<(), StreamErrorIncoming> {
        // The generic `WriteBuf<B>` stays on this frontend stack; only its length
        // classification enters the reducer. Owned bytes are materialized solely
        // when the reducer returns `SubmitSend`, so nothing is copied on a
        // terminal / misuse / oversized / empty path.
        let mut wb: h3::quic::WriteBuf<B> = data.into();
        let payload = classify_payload(wb.remaining());
        let mut input = SendInput::SendRequested {
            payload,
            terminal: self.sctx.resolve_terminal(),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::ReturnSent => return Ok(()),
                SendCommand::ReturnImmediateError(e) => return Err(convert_send_op(e)),
                SendCommand::ReturnError(t) => {
                    // Delivery point: commit the connection slot on the way out.
                    return Err(convert_send(self.sctx.commit_send_winner(t)));
                }
                SendCommand::SubmitSend => {
                    // The single production copy path: materialize the complete
                    // wire representation into owned `Bytes` while `B` is still on
                    // this thread, then hand it to the seam (which reclaims the box
                    // itself on an immediate `Err`). `&mut WriteBuf<B>` is a `Buf`,
                    // so this drives the exact `copy_to_bytes` allocation the
                    // Criterion bench measures through `bench_support`.
                    let buf = copy_into_send_buffer(&mut wb);
                    let res = self.exec.submit_send(buf);
                    input = SendInput::SendSubmitted {
                        result: res,
                        terminal: self.sctx.resolve_terminal(),
                    };
                }
                SendCommand::PublishTerminal {
                    candidate,
                    continuation,
                } => {
                    let winner = publish_send(&self.sctx.send_terminal, candidate);
                    input = SendInput::TerminalPublished {
                        winner,
                        continuation,
                    };
                }
                _ => {
                    return Err(convert_send(SendTerminal::Internal(
                        "unexpected send_data command",
                    )));
                }
            }
        }
    }

    // Send FIN signal to peer (graceful shutdown), idempotently.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_finish(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), StreamErrorIncoming>> {
        use std::task::Poll;
        let mut input = SendInput::PollFinish {
            poll: self.poll_send_channel(cx),
            terminal: self.sctx.resolve_terminal(),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::Pending => return Poll::Pending,
                SendCommand::ReturnFinished => return Poll::Ready(Ok(())),
                SendCommand::ReturnError(t) => {
                    // Delivery point: commit the connection slot on the way out.
                    // A graceful `ReturnFinished` never reaches here, so a
                    // provisional cause prepared as input is never committed (MF-2).
                    return Poll::Ready(Err(convert_send(self.sctx.commit_send_winner(t))));
                }
                SendCommand::SubmitGraceful => {
                    let res = self.exec.submit_graceful();
                    input = SendInput::GracefulSubmitted {
                        result: res,
                        terminal: self.sctx.resolve_terminal(),
                    };
                }
                SendCommand::RepollFinish => {
                    // Re-poll the channel in THIS call so a synchronously-queued
                    // `FinishComplete` is drained (or the waker re-armed) — never
                    // defer to a later poll (that recreates the lost-waker bug).
                    input = SendInput::PollFinish {
                        poll: self.poll_send_channel(cx),
                        terminal: self.sctx.resolve_terminal(),
                    };
                }
                SendCommand::PublishTerminal {
                    candidate,
                    continuation,
                } => {
                    let winner = publish_send(&self.sctx.send_terminal, candidate);
                    input = SendInput::TerminalPublished {
                        winner,
                        continuation,
                    };
                }
                _ => {
                    return Poll::Ready(Err(convert_send(SendTerminal::Internal(
                        "unexpected poll_finish command",
                    ))));
                }
            }
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn reset(&mut self, reset_code: u64) {
        // Infallible: issues at most one native `RESET_STREAM` with a clamped code
        // through the seam, records the terminal for a later poll, and clears the
        // reset reservation. A callback-published peer/connection terminal wins
        // over this local reset (no second native op).
        //
        // `reset()` returns NO observable terminal to the caller, so it reads the
        // send winner through the non-freezing `resolve_terminal` and NEVER commits
        // the shared connection slot: freezing here would prematurely retain a
        // provisional `LocalClose` over a later peer/transport refinement that a
        // subsequent genuine caller observation should surface.
        let mut input = SendInput::Reset {
            code: clamp_application_code(reset_code),
            terminal: self.sctx.resolve_terminal(),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::NoOp => return, // already terminal / published
                SendCommand::SubmitReset(c) => {
                    let res = self.exec.submit_reset(c);
                    input = SendInput::ResetSubmitted {
                        code: c,
                        result: res,
                        terminal: self.sctx.resolve_terminal(),
                    };
                }
                SendCommand::PublishTerminal {
                    candidate,
                    continuation,
                } => {
                    let winner = publish_send(&self.sctx.send_terminal, candidate);
                    input = SendInput::TerminalPublished {
                        winner,
                        continuation,
                    };
                }
                // reset consumes any terminal; nothing to surface to h3 now.
                _ => return,
            }
        }
    }

    fn send_id(&self) -> h3::quic::StreamId {
        // Cached at construction; no native query, so this never panics.
        self.sctx.id
    }
}

impl H3SendStream {
    /// Poll the single ordered send-event channel (MF-1) into a [`SendPoll`].
    ///
    /// A closed channel with no queued event is an adapter-internal fault
    /// (`SendPoll::Closed`), distinct from `Pending`; the reducer surfaces a
    /// published terminal winner ahead of it, so a normal connection-shutdown
    /// close still yields the terminal reason rather than `Closed`.
    fn poll_send_channel(&mut self, cx: &mut std::task::Context<'_>) -> SendPoll {
        match self.sctx.send.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(ev)) => SendPoll::Event(ev),
            std::task::Poll::Ready(None) => SendPoll::Closed,
            std::task::Poll::Pending => SendPoll::Pending,
        }
    }
}

impl SendStreamReceiveCtx {
    /// Load the current send winner **without freezing** the shared connection
    /// slot, resolving connection precedence for every send-slot state so a
    /// recorded connection cause is never masked (the MF-1 fix).
    ///
    /// This is the single, non-freezing reducer-input read for all four send
    /// frontends (`send_data`, `poll_ready`, `poll_finish`, `reset`). It defines
    /// the connection-fallback precedence:
    /// - `Some(Connection(fallback))` → the connection marker enriched from the
    ///   shared slot (`peek_conn_terminal`), the carried reason only a defensive
    ///   fallback;
    /// - `None` → **MF-1 fallback**: a recorded connection cause is surfaced as
    ///   `Connection(cause)`, so an immediate native error in the window before the
    ///   stream callback publishes its marker still delivers the specific cause;
    /// - `Some(ProvisionalAbort)` → **provisional-winner fallback**: a recorded
    ///   connection cause takes precedence over the still-refinable provisional
    ///   marker (returned as `Connection(cause)`); with no connection cause the
    ///   provisional marker is kept (never surfaced, still refinable);
    /// - `Some(other)` (authoritative `Stopped` / `LocalReset` / `Failed` /
    ///   `Internal`) → returned unchanged.
    ///
    /// The rule: peek the shared connection slot as a fallback whenever the send
    /// slot does not already hold an authoritative send-scope winner. Freezing is
    /// deferred to [`Self::commit_send_winner`] at the delivery point.
    fn resolve_terminal(&self) -> Option<SendTerminal> {
        match load_winner(&self.send_terminal) {
            Some(SendTerminal::Connection(fallback)) => Some(SendTerminal::Connection(
                peek_conn_terminal(&self.conn_terminal).unwrap_or(fallback),
            )),
            Some(SendTerminal::ProvisionalAbort) => match peek_conn_terminal(&self.conn_terminal) {
                Some(reason) => Some(SendTerminal::Connection(reason)),
                None => Some(SendTerminal::ProvisionalAbort),
            },
            None => peek_conn_terminal(&self.conn_terminal).map(SendTerminal::Connection),
            other => other,
        }
    }

    /// Commit-on-delivery: freeze the shared connection slot at the exact point a
    /// connection cause is returned to h3, and only when a cause is present.
    ///
    /// A [`SendTerminal::Connection`] winner is resolved+frozen through
    /// [`commit_conn`] (the carried reason only a defensive fallback if the shared
    /// slot was somehow never recorded); every other winner is returned unchanged
    /// and never freezes the connection slot, keeping send-scope and
    /// connection-scope separation intact. `reset()` returns no observable terminal
    /// and therefore never calls this (never freezes) — the MF-2 fix.
    fn commit_send_winner(&self, w: SendTerminal) -> SendTerminal {
        match w {
            SendTerminal::Connection(fallback) => {
                SendTerminal::Connection(commit_conn(&self.conn_terminal).unwrap_or(fallback))
            }
            other => other,
        }
    }
}

/// Classify a `send_data` payload length into a [`SendPayload`] before any
/// allocation. The `NonEmpty` upper bound is `MAX_ADAPTER_SEND` (< `u32::MAX`),
/// so the `as u32` cast never truncates.
fn classify_payload(remaining: usize) -> SendPayload {
    match classify_send_len(remaining) {
        SendLen::Empty => SendPayload::Empty,
        SendLen::Oversized => SendPayload::Oversized { len: remaining },
        SendLen::NonEmpty => SendPayload::NonEmpty {
            len: remaining as u32,
        },
    }
}

impl RecvStreamReceiveCtx {
    /// SF-6: mark the receive half as locally ended by OUR OWN `stop_sending`.
    ///
    /// Idempotent and deliberately independent of any FFI outcome: the h3
    /// `stop_sending` trait method is infallible, so this sticky local
    /// end-of-stream must be set regardless of whether the `ABORT_RECEIVE`
    /// submit succeeds. Once set, `poll_event` returns a clean `Ok(None)` and
    /// leaves the sticky `terminal` slot untouched.
    fn close_receive_locally(&mut self) {
        self.receive_closed = true;
    }

    /// Drain one explicit receive event and map it to the h3 `poll_data` result.
    ///
    /// Sticky: once a terminal is stored, every later poll returns the same
    /// class without touching the channel. SF-6: a local `stop_sending` sets
    /// `receive_closed`, which yields a clean `Ok(None)` end-of-stream and leaves
    /// the terminal slot untouched.
    fn poll_event(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
        use std::task::Poll;
        // SF-6: our own `stop_sending` ended the receive half locally. This is a
        // clean end-of-stream, NOT a terminal: `terminal` stays untouched.
        if self.receive_closed {
            return Poll::Ready(Ok(None));
        }
        // Replay the stored sticky terminal without re-polling the channel.
        if let Some(terminal) = &self.terminal {
            return Poll::Ready(convert_recv(terminal.clone()));
        }
        match ready!(self.receive.poll_next_unpin(cx)) {
            Some(ReceiveEvent::Data(b)) => Poll::Ready(Ok(Some(b))),
            Some(ReceiveEvent::Fin) => self.store_and_convert(ReceiveTerminal::Fin),
            Some(ReceiveEvent::Reset(code)) => self.store_and_convert(ReceiveTerminal::Reset(code)),
            Some(ReceiveEvent::Connection(fallback)) => {
                // Observation point (FR-013): resolve *and freeze* the SHARED
                // connection winner (a refinement landing before now is honoured;
                // one landing after is rejected) and cache it as the sticky recv
                // terminal, so subsequent polls replay the same frozen value. The
                // carried `fallback` is only used if the shared slot was somehow
                // never recorded (the callback always records before signalling).
                let winner = observe_conn_winner(&self.conn_terminal).unwrap_or(fallback);
                self.store_and_convert(ReceiveTerminal::Connection(winner))
            }
            // A STREAM-LOCAL internal fault (e.g. a contained callback panic on
            // this stream). It maps DIRECTLY to a stream-scoped
            // `ReceiveTerminal::Internal` and deliberately does NOT call
            // `observe_conn_winner`, so it never freezes the shared connection
            // slot on the EMPTY value — a later genuine connection cause is still
            // delivered to the accept/open frontends (F-A / MF-1 / SF-C).
            Some(ReceiveEvent::Internal(msg)) => {
                self.store_and_convert(ReceiveTerminal::Internal(msg))
            }
            // A closed channel without an explicit terminal event is an internal
            // fault, not a clean end-of-stream.
            None => self.store_and_convert(ReceiveTerminal::Internal(
                "receive channel closed without a terminal reason",
            )),
        }
    }

    /// Store `terminal` as the sticky receive reason and convert it for h3.
    fn store_and_convert(
        &mut self,
        terminal: ReceiveTerminal,
    ) -> std::task::Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
        self.terminal = Some(terminal.clone());
        std::task::Poll::Ready(convert_recv(terminal))
    }
}

impl RecvStream for H3RecvStream {
    type Buf = Bytes;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_data(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        let polled = self.rctx.poll_event(cx);
        // On delivering `Data(b)` of length L, perform ONE `RecvState` locked
        // transition: `buffered -= L` → decide-complete. If a pend is
        // outstanding and buffered has fallen below the bound, complete the
        // pended indication on THIS stream with its FULL saved length (never the
        // drained length), re-arming its receive callbacks and reopening its
        // flow-control window (SF-A).
        if let std::task::Poll::Ready(Ok(Some(b))) = &polled
            && let Some(len) = self.rctx.recv_budget.on_drained(b.len())
        {
            self.exec.submit_receive_complete(len);
        }
        polled
    }

    /// Stop accepting data. Discard unread data, notify peer to not send.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn stop_sending(&mut self, error_code: u64) {
        // Close the send path (code clamped here through the seam, Phase 2/7).
        let _ = self
            .exec
            .submit_stop_sending(clamp_application_code(error_code));
        // SF-6: sticky local end-of-stream. Subsequent `poll_data` returns a
        // clean `Ok(None)` and injects NO receive terminal (`terminal`
        // untouched). Set unconditionally via the local-state seam: the FFI
        // submit above is best-effort and its outcome must not gate the
        // infallible h3 `stop_sending` contract.
        self.rctx.close_receive_locally();
    }

    fn recv_id(&self) -> h3::quic::StreamId {
        // Cached at construction; no native query, so this never panics.
        self.rctx.id
    }
}

// bidi stream

impl<B: Buf> SendStream<B> for H3Stream {
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), StreamErrorIncoming>> {
        SendStream::<B>::poll_ready(&mut self.send, cx)
    }

    fn send_data<T: Into<h3::quic::WriteBuf<B>>>(
        &mut self,
        data: T,
    ) -> Result<(), StreamErrorIncoming> {
        SendStream::<B>::send_data(&mut self.send, data)
    }

    fn poll_finish(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), StreamErrorIncoming>> {
        SendStream::<B>::poll_finish(&mut self.send, cx)
    }

    fn reset(&mut self, reset_code: u64) {
        SendStream::<B>::reset(&mut self.send, reset_code);
    }

    fn send_id(&self) -> h3::quic::StreamId {
        SendStream::<B>::send_id(&self.send)
    }
}

impl RecvStream for H3Stream {
    type Buf = Bytes;

    fn poll_data(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        RecvStream::poll_data(&mut self.recv, cx)
    }

    fn stop_sending(&mut self, error_code: u64) {
        RecvStream::stop_sending(&mut self.recv, error_code)
    }

    fn recv_id(&self) -> h3::quic::StreamId {
        RecvStream::recv_id(&self.recv)
    }
}

impl<B: Buf> BidiStream<B> for H3Stream {
    type SendStream = H3SendStream;

    type RecvStream = H3RecvStream;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use bytes::Buf;
    use h3::error::ConnectionError;
    use http::Uri;
    use msquic::{BufferRef, CredentialConfig, CredentialFlags, RegistrationConfig, Settings};

    use crate::Connection;

    pub mod util {
        use msquic::Credential;
        // used for debugging
        pub const DEVEL_TRACE_LEVEL: tracing::Level = tracing::Level::TRACE;

        pub fn try_setup_tracing() {
            let _ = tracing_subscriber::fmt()
                .with_max_level(DEVEL_TRACE_LEVEL)
                .try_init();
        }

        /// Use pwsh to get the test cert hash
        #[cfg(target_os = "windows")]
        pub fn get_test_cred() -> Credential {
            use msquic::CertificateHash;

            let output = std::process::Command::new("pwsh.exe")
                .args(["-Command", "Get-ChildItem Cert:\\CurrentUser\\My | Where-Object -Property FriendlyName -EQ -Value MsQuic-Test | Select-Object -ExpandProperty Thumbprint -First 1"]).
                output().expect("Failed to execute command");
            assert!(output.status.success());
            let mut s = String::from_utf8(output.stdout).unwrap();
            if s.ends_with('\n') {
                s.pop();
                if s.ends_with('\r') {
                    s.pop();
                }
            };
            Credential::CertificateHash(CertificateHash::from_str(&s).unwrap())
        }

        /// Generate a test cert if not present using openssl cli.
        #[cfg(not(target_os = "windows"))]
        pub fn get_test_cred() -> Credential {
            use msquic::CertificateFile;

            // Serialize cert generation across parallel tests in the same
            // process. Without this, two tests racing on the shared cert dir
            // can delete/recreate it out from under each other's `openssl`
            // invocation, which then fails to spawn with NotFound.
            static CERT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _lock = CERT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

            let cert_dir = std::env::temp_dir().join("msquic_h3_test_rs");
            let key = "key.pem";
            let cert = "cert.pem";
            let key_path = cert_dir.join(key);
            let cert_path = cert_dir.join(cert);
            if !key_path.exists() || !cert_path.exists() {
                // remove the dir
                let _ = std::fs::remove_dir_all(&cert_dir);
                std::fs::create_dir_all(&cert_dir).expect("cannot create cert dir");
                // generate test cert using openssl cli
                let output = std::process::Command::new("openssl")
                    .args([
                        "req",
                        "-x509",
                        "-newkey",
                        "rsa:4096",
                        "-keyout",
                        "key.pem",
                        "-out",
                        "cert.pem",
                        "-sha256",
                        "-days",
                        "3650",
                        "-nodes",
                        "-subj",
                        "/CN=localhost",
                    ])
                    .current_dir(&cert_dir)
                    .stderr(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .output()
                    .expect("cannot generate cert");
                if !output.status.success() {
                    panic!("generate cert failed");
                }
            }
            Credential::CertificateFile(CertificateFile::new(
                key_path.display().to_string(),
                cert_path.display().to_string(),
            ))
        }
    }

    pub(crate) async fn send_get_request(uri: Uri) {
        let app_name = String::from("testapp");
        let config = RegistrationConfig::new().set_app_name(app_name);
        let reg = Arc::new(crate::Registration::new(&config).unwrap());

        let alpn = BufferRef::from("h3");
        // create an client
        // open client
        let client_settings = Settings::new().set_IdleTimeoutMs(2000);
        let client_config = reg
            .open_configuration(&[alpn], Some(&client_settings))
            .unwrap();
        {
            let cred_config = CredentialConfig::new_client()
                .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION);
            client_config.load_credential(&cred_config).unwrap();
        }

        tracing::info!("client conn open and start");
        let conn = Connection::connect(
            &reg,
            &client_config,
            uri.host().unwrap(),
            uri.port_u16().unwrap(),
        )
        .await
        .unwrap();

        tracing::info!("client create h3 client");
        let (mut driver, mut send_request) = h3::client::new(conn).await.unwrap();

        tracing::info!("client start driver");
        let drive = async move {
            Err::<(), ConnectionError>(futures::future::poll_fn(|cx| driver.poll_close(cx)).await)
        };

        // tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        // In the following block, we want to take ownership of `send_request`:
        // the connection will be closed only when all `SendRequest`s instances
        // are dropped.
        //
        //             So we "move" it.
        //                  vvvv
        let request = async move {
            tracing::info!("sending request ...");

            let req = http::Request::builder().uri(uri).body(())?;

            // sending request results in a bidirectional stream,
            // which is also used for receiving response
            let mut stream = send_request.send_request(req).await?;

            // finish on the sending side
            stream.finish().await?;

            tracing::info!("receiving response ...");

            let resp = stream.recv_response().await?;

            tracing::info!("response: {:?} {}", resp.version(), resp.status());
            tracing::info!("headers: {:#?}", resp.headers());

            // `recv_data()` must be called after `recv_response()` for
            // receiving potential response body
            let mut data = vec![];
            while let Some(mut chunk) = stream.recv_data().await? {
                // let mut out = tokio::io::stdout();
                // tokio::io::AsyncWriteExt::write_all_buf(&mut out, &mut chunk).await?;
                // tokio::io::AsyncWriteExt::flush(&mut out).await?;
                let mut dst = vec![0; chunk.remaining()];
                chunk.copy_to_slice(&mut dst[..]);
                data.extend_from_slice(&dst);
            }
            let body = String::from_utf8_lossy(&data);
            tracing::info!("client got body: {}", body);
            // tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok::<_, Box<dyn std::error::Error>>(())
        };

        let (req_res, drive_res) = tokio::join!(request, drive);
        if let Err(e) = req_res {
            tracing::error!("req_err {e:?}");
        }
        if let Err(e) = drive_res {
            tracing::error!("drive_res {e:?}");
        }
        tracing::info!("client ended success");

        // Exercise the teardown contract: after the driver ended and the h3
        // client (owning the Connection) was dropped, shutdown + wait_idle must
        // resolve once every connection handle has closed. A timeout here would
        // signal the RegistrationClose-blocking hang this feature prevents.
        reg.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(5), reg.wait_idle())
            .await
            .expect("wait_idle should resolve after the connection closed");
    }

    /// Manual-only external smoke check (MF-3): drives a real HTTP/3 GET against a
    /// public third-party endpoint (`h2o.examp1e.net`). It depends on DNS, remote
    /// uptime, and ALPN/cert behavior, so it is `#[ignore]`d to keep the default
    /// suite hermetic. Run it explicitly with
    /// `cargo test --no-default-features --features native-find -- --ignored client_test_apache`
    /// when networking is available. The loopback `conformance` suite covers the
    /// client path hermetically.
    #[test]
    #[ignore = "requires external internet access (h2o.examp1e.net); run manually with --ignored"]
    fn client_test_apache() {
        util::try_setup_tracing();
        // This does not work (cloudflare servers):
        // let uri = http::Uri::from_static("https://quic.tech:8443/");
        // let uri = http::Uri::from_static("https://cloudflare-quic.com:443/");

        // These works
        let uri = http::Uri::from_static("https://h2o.examp1e.net:443");
        // let uri = http::Uri::from_static("https://docs.trafficserver.apache.org:443/");
        // use tokio
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(send_get_request(uri));
    }
}

/// Phase 4 (explicit receive events) unit tests: prove that a graceful FIN, a
/// peer reset, an empty non-FIN notification, a peer send-shutdown, a
/// connection failure, and a local `stop_sending` are all observably distinct at
/// the `poll_data` boundary. Hermetic (no network): events are driven straight
/// through `stream_callback` and drained via `RecvStreamReceiveCtx::poll_event`,
/// so no native `msquic::Stream` is required.
#[cfg(test)]
mod receive_events {
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use h3::quic::{ConnectionErrorIncoming, RecvStream, StreamErrorIncoming};

    use crate::error::ConnectionTerminal;
    use crate::msquic::{BufferRef, ReceiveFlags, Status, StatusCode, StreamEvent};
    use crate::{
        H3RecvStream, MAX_RECV_BUFFER, NoShutdown, RecvBudget, RecvExec, RecvStreamReceiveCtx,
        new_conn_terminal_slot, record_conn_terminal, stream_callback, stream_ctx_channel,
        stream_ctx_channel_with_conn, stream_recover,
    };

    fn noop_context() -> Context<'static> {
        // A leaked no-op waker gives a `'static` context usable across polls.
        let waker = Box::leak(Box::new(futures::task::noop_waker()));
        Context::from_waker(waker)
    }

    /// Drive one `StreamEvent::Receive` carrying `data` and `flags`.
    fn feed_receive(ctx: &mut crate::StreamSendCtx, data: &[u8], flags: ReceiveFlags) {
        let bufs = [BufferRef::from(data)];
        let mut total = data.len() as u64;
        let ev = StreamEvent::Receive {
            absolute_offset: 0,
            total_buffer_length: &mut total,
            buffers: &bufs,
            flags,
        };
        assert!(stream_callback(ctx, ev).is_ok());
    }

    fn poll(rrx: &mut RecvStreamReceiveCtx) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
        let mut cx = noop_context();
        rrx.poll_event(&mut cx)
    }

    #[test]
    fn graceful_fin_yields_data_then_clean_eof() {
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        feed_receive(&mut ctx, &[1, 2, 3], ReceiveFlags::FIN);
        // Data first.
        match poll(&mut rrx) {
            Poll::Ready(Ok(Some(b))) => assert_eq!(&b[..], &[1, 2, 3]),
            other => panic!("expected data, got {other:?}"),
        }
        // Then a clean end-of-stream.
        assert!(matches!(poll(&mut rrx), Poll::Ready(Ok(None))));
        // Sticky: a later poll keeps returning clean EOF.
        assert!(matches!(poll(&mut rrx), Poll::Ready(Ok(None))));
    }

    #[test]
    fn peer_reset_yields_stream_terminated_with_code() {
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        assert!(stream_callback(&mut ctx, StreamEvent::PeerSendAborted { error_code: 42 }).is_ok());
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(error_code, 42)
            }
            other => panic!("expected StreamTerminated{{42}}, got {other:?}"),
        }
    }

    #[test]
    fn graceful_fin_and_peer_reset_are_observably_different() {
        // FIN => Ok(None); RESET_STREAM => Err(StreamTerminated). The two paths
        // must not be conflated (Finding 1).
        let (mut fin_ctx, _s1, mut fin_rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        feed_receive(&mut fin_ctx, &[], ReceiveFlags::FIN);
        let fin = poll(&mut fin_rrx);

        let (mut rst_ctx, _s2, mut rst_rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        assert!(
            stream_callback(&mut rst_ctx, StreamEvent::PeerSendAborted { error_code: 7 }).is_ok()
        );
        let rst = poll(&mut rst_rrx);

        assert!(matches!(fin, Poll::Ready(Ok(None))));
        assert!(matches!(
            rst,
            Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code: 7 }))
        ));
    }

    #[test]
    fn empty_non_fin_receive_produces_no_event() {
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        // A zero-length, non-FIN notification is not an end-of-stream (Finding 8).
        feed_receive(&mut ctx, &[], ReceiveFlags::NONE);
        // No event was enqueued and the channel is still open: Pending.
        assert!(matches!(poll(&mut rrx), Poll::Pending));
        // The terminal slot must remain empty.
        assert!(rrx.terminal.is_none());
    }

    #[test]
    fn peer_send_shutdown_yields_clean_eof() {
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        assert!(stream_callback(&mut ctx, StreamEvent::PeerSendShutdown).is_ok());
        assert!(matches!(poll(&mut rrx), Poll::Ready(Ok(None))));
    }

    #[test]
    fn first_receive_terminal_wins() {
        // The usual `Receive{FIN}` then `PeerSendShutdown` sequence must yield a
        // single clean EOF, not a duplicate. Likewise a reset published first is
        // not overwritten by a later FIN.
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        assert!(stream_callback(&mut ctx, StreamEvent::PeerSendAborted { error_code: 9 }).is_ok());
        // A later graceful FIN must NOT override the earlier reset.
        assert!(stream_callback(&mut ctx, StreamEvent::PeerSendShutdown).is_ok());
        assert!(matches!(
            poll(&mut rrx),
            Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code: 9 }))
        ));
        // Sticky reset persists.
        assert!(matches!(
            poll(&mut rrx),
            Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code: 9 }))
        ));
    }

    #[test]
    fn connection_shutdown_surfaces_connection_error() {
        // A peer application close (by_app && closed_remotely) delivered as a
        // connection-caused stream shutdown surfaces the connection error.
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        let ev = StreamEvent::ShutdownComplete {
            connection_shutdown: true,
            app_close_in_progress: false,
            connection_shutdown_by_app: true,
            connection_closed_remotely: true,
            connection_error_code: 13,
            connection_close_status: Status::new(StatusCode::QUIC_STATUS_ABORTED),
        };
        assert!(stream_callback(&mut ctx, ev).is_ok());
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 13),
            other => panic!("expected connection ApplicationClose{{13}}, got {other:?}"),
        }
    }

    #[test]
    fn stream_local_shutdown_publishes_no_connection_terminal() {
        // A stream-local `ShutdownComplete` (connection_shutdown == false) must
        // NOT surface a connection error. The channel closes with no terminal
        // event, which maps to the internal fault class.
        let (mut ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        let ev = StreamEvent::ShutdownComplete {
            connection_shutdown: false,
            app_close_in_progress: false,
            connection_shutdown_by_app: false,
            connection_closed_remotely: false,
            connection_error_code: 0,
            connection_close_status: Status::new(StatusCode::QUIC_STATUS_SUCCESS),
        };
        assert!(stream_callback(&mut ctx, ev).is_ok());
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(_),
            })) => {}
            other => panic!("expected internal-fault on bare channel close, got {other:?}"),
        }
    }

    #[test]
    fn poll_data_after_local_stop_sending_is_ok_none() {
        // SF-6 / FR-017: after a local `stop_sending`, `poll_data` returns a
        // defined clean end-of-stream (Ok(None)), NOT a peer error, and no
        // receive terminal is injected. This drives the REAL local-state seam
        // that `RecvStream::stop_sending` calls (`close_receive_locally`) rather
        // than poking the field directly, so it genuinely covers the
        // stop_sending local-EOF behavior without needing a live FFI stream.
        // `stop_sending` separately submits `ABORT_RECEIVE` with the clamped
        // application code; that FFI effect is exercised by the loopback tests.
        let (_ctx, _srx, mut rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        assert!(!rrx.receive_closed);
        rrx.close_receive_locally(); // the exact call RecvStream::stop_sending makes
        assert!(matches!(poll(&mut rrx), Poll::Ready(Ok(None))));
        // No terminal was injected: the sticky recv terminal slot stays empty.
        assert!(rrx.terminal.is_none());
        assert!(rrx.receive_closed);
    }

    /// A connection-caused stream `ShutdownComplete` whose derived fallback is the
    /// PROVISIONAL `LocalClose` (by_app && !closed_remotely). The callback records
    /// this into the shared connection slot and enqueues the receive marker.
    fn provisional_local_close(ctx: &mut crate::StreamSendCtx) {
        let ev = StreamEvent::ShutdownComplete {
            connection_shutdown: true,
            app_close_in_progress: false,
            connection_shutdown_by_app: true,
            connection_closed_remotely: false,
            connection_error_code: 0,
            connection_close_status: Status::new(StatusCode::QUIC_STATUS_ABORTED),
        };
        assert!(stream_callback(ctx, ev).is_ok());
    }

    #[test]
    fn f1_recv_refinement_before_observation_surfaces_refined_cause() {
        // FR-013: a provisional connection fallback (LocalClose) recorded by the
        // stream callback is refined on the SHARED slot to a specific peer cause
        // BEFORE the stream observes it at poll_data → poll_data reports the
        // REFINED cause (ApplicationClose), not the stale LocalClose.
        let slot = new_conn_terminal_slot();
        let (mut ctx, _srx, mut rrx) =
            stream_ctx_channel_with_conn(4u64.try_into().unwrap(), slot.clone());
        provisional_local_close(&mut ctx);
        // A concurrent connection-callback refinement lands before observation.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));

        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9),
            other => panic!("expected refined ApplicationClose{{9}}, got {other:?}"),
        }
        // The shared slot is now frozen: a later refinement is rejected.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(11));
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9, "sticky: the first observed cause persists"),
            other => panic!("expected stable ApplicationClose{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn f1_recv_refinement_after_observation_is_rejected() {
        // FR-013 freeze-on-observation: once poll_data has OBSERVED the connection
        // winner (freezing the shared slot), a later refinement is rejected and the
        // stream keeps reporting the observed value.
        let slot = new_conn_terminal_slot();
        let (mut ctx, _srx, mut rrx) =
            stream_ctx_channel_with_conn(4u64.try_into().unwrap(), slot.clone());
        provisional_local_close(&mut ctx);
        // Observe FIRST — this freezes the shared slot at the provisional LocalClose.
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::Undefined(e),
            })) => assert!(
                e.downcast_ref::<crate::LocalConnectionClose>().is_some(),
                "expected LocalConnectionClose, got {e:?}"
            ),
            other => panic!("expected LocalConnectionClose (Undefined), got {other:?}"),
        }
        // A refinement AFTER observation is rejected (frozen); the stream still
        // reports the observed LocalClose.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::Undefined(e),
            })) => assert!(
                e.downcast_ref::<crate::LocalConnectionClose>().is_some(),
                "post-observation refinement must be rejected, got {e:?}"
            ),
            other => panic!("expected frozen LocalConnectionClose, got {other:?}"),
        }
    }

    #[test]
    fn f_a_stream_local_panic_does_not_freeze_empty_connection_slot() {
        // F-A regression: a contained stream-callback panic recovers via
        // `stream_recover`, which publishes a STREAM-LOCAL internal terminal
        // (`ReceiveEvent::Internal`) and must NOT touch the shared connection
        // slot. Polling the recovered stream surfaces the internal fault, and a
        // later GENUINE connection cause recorded on the shared slot is still
        // refinable and delivered to a subsequent observer — proving the slot was
        // not frozen EMPTY (regression of MF-1 / SF-C / FR-002 / FR-003).
        let slot = new_conn_terminal_slot();
        let (mut ctx, _srx, mut rrx) =
            stream_ctx_channel_with_conn(4u64.try_into().unwrap(), slot.clone());

        // Recover a stream via a contained panic (no native handle: no-op seam).
        assert!(stream_recover(&mut ctx, &NoShutdown).is_err());

        // The recovered receive half reports the STREAM-LOCAL internal fault.
        match poll(&mut rrx) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(_),
            })) => {}
            other => panic!("expected stream-local InternalError, got {other:?}"),
        }

        // The shared connection slot was NOT frozen by the stream-local panic: a
        // genuine peer/transport connection cause recorded afterwards is still
        // observable (not discarded, not overridden by an internal fault). A fresh
        // stream sharing the SAME slot observes the delivered cause.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        let (mut ctx2, _srx2, mut rrx2) =
            stream_ctx_channel_with_conn(8u64.try_into().unwrap(), slot.clone());
        provisional_local_close(&mut ctx2);
        match poll(&mut rrx2) {
            Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(
                error_code, 9,
                "the genuine connection cause survives the stream-local panic"
            ),
            other => panic!("expected delivered ApplicationClose{{9}}, got {other:?}"),
        }
    }

    // ----- Phase 2: per-stream receive backpressure (SF-A) -----

    /// Recording receive-side seam: captures every `receive_complete` length (and
    /// any `stop_sending` code) issued on THIS stream, so the per-stream
    /// pending→complete cycle is asserted against the call log, not timing.
    #[derive(Debug, Default, Clone)]
    struct RecordingRecvExec {
        completes: Arc<Mutex<Vec<u64>>>,
        stops: Arc<Mutex<Vec<u64>>>,
    }

    impl RecvExec for RecordingRecvExec {
        fn submit_stop_sending(&self, code: u64) -> Result<(), Status> {
            self.stops.lock().unwrap().push(code);
            Ok(())
        }
        fn submit_receive_complete(&self, len: u64) {
            self.completes.lock().unwrap().push(len);
        }
    }

    /// Drive one `StreamEvent::Receive` and RETURN the callback result (so a
    /// `QUIC_STATUS_PENDING` return is observable).
    fn feed_receive_result(
        ctx: &mut crate::StreamSendCtx,
        data: &[u8],
        flags: ReceiveFlags,
    ) -> Result<(), Status> {
        let bufs = [BufferRef::from(data)];
        let mut total = data.len() as u64;
        let ev = StreamEvent::Receive {
            absolute_offset: 0,
            total_buffer_length: &mut total,
            buffers: &bufs,
            flags,
        };
        stream_callback(ctx, ev)
    }

    fn is_pending(res: &Result<(), Status>) -> bool {
        matches!(res, Err(s) if s.try_as_status_code() == Ok(StatusCode::QUIC_STATUS_PENDING))
    }

    /// SC-004 primary (hermetic, per-stream, faithful flood): a slow ("stalled")
    /// consumer that lags the sender. The peer genuinely offers ≥ 8× the
    /// per-stream bound THROUGH the real receive callback/seam on ONE stream;
    /// every over-budget indication returns `QUIC_STATUS_PENDING` (modelling
    /// native pausing the stream until `receive_complete`), and the lagging
    /// consumer drains just enough to re-arm. Across the entire flood the
    /// measured buffered PEAK stays ≤ `MAX_RECV_BUFFER` + one indication — the
    /// peak comes from REAL offers that returned PENDING, not from a counter.
    #[test]
    fn stalled_receive_is_bounded() {
        const CHUNK: usize = 300 * 1024; // 300 KiB indications

        let (mut ctx, _sctx, rctx) = stream_ctx_channel(0u64.try_into().unwrap());
        let rec = RecordingRecvExec::default();
        let completes = rec.completes.clone();
        let mut recv = H3RecvStream::with_exec(Box::new(rec), rctx);
        let mut cx = noop_context();

        let offer_target = 8 * MAX_RECV_BUFFER; // >= 8 MiB genuinely offered
        let data = vec![0xC3u8; CHUNK];

        let mut offered = 0usize;
        let mut peak = 0usize;
        let mut pend_returns = 0usize;
        while offered < offer_target {
            if recv.rctx.recv_budget.pend_outstanding() {
                // Native has paused THIS stream (the callback returned PENDING and
                // will not be called again until re-armed): no further indication
                // is delivered. The lagging consumer drains exactly ONE buffered
                // indication, which re-arms the window via the RecvExec seam.
                match recv.poll_data(&mut cx) {
                    Poll::Ready(Ok(Some(_))) => {}
                    other => panic!("expected buffered data while stalled, got {other:?}"),
                }
                continue;
            }
            // A REAL offer through the real receive callback/seam.
            let res = feed_receive_result(&mut ctx, &data, ReceiveFlags::NONE);
            offered += CHUNK;
            peak = peak.max(recv.rctx.recv_budget.buffered());
            if is_pending(&res) {
                pend_returns += 1;
                assert!(recv.rctx.recv_budget.pend_outstanding());
                // The saved length is this FULL indication (never a drained one).
                assert_eq!(
                    recv.rctx.recv_budget.pending_indication_len() as usize,
                    CHUNK
                );
            } else {
                assert!(res.is_ok(), "sub-bound offer must be Ok, got {res:?}");
            }
        }

        // Genuinely offered >= 8x the bound, and the peer really hit the bound.
        assert!(offered >= offer_target);
        assert!(pend_returns > 0, "over-budget offers must return PENDING");
        // The flood may end on a PENDING offer; drain the remaining buffered data
        // so that final pend is completed too (each pend gets exactly one
        // completion). This never exceeds the peak already measured.
        while recv.rctx.recv_budget.pend_outstanding() {
            match recv.poll_data(&mut cx) {
                Poll::Ready(Ok(Some(_))) => {}
                other => panic!("expected buffered data while draining final pend, got {other:?}"),
            }
        }
        println!(
            "stalled_receive_is_bounded: offered={offered} bound={MAX_RECV_BUFFER} \
             chunk={CHUNK} peak={peak} pend_returns={pend_returns} completes={}",
            completes.lock().unwrap().len()
        );
        // Bounded peak: it reached the bound but never exceeded bound + one
        // in-flight indication, independent of how much was offered.
        assert!(
            peak >= MAX_RECV_BUFFER,
            "peak {peak} should reach the bound"
        );
        assert!(
            peak <= MAX_RECV_BUFFER + CHUNK,
            "peak {peak} must stay <= bound + one indication ({})",
            MAX_RECV_BUFFER + CHUNK
        );
        // Each PENDING was completed exactly once via the seam, always with the
        // FULL saved indication length.
        let c = completes.lock().unwrap();
        assert_eq!(
            c.len(),
            pend_returns,
            "exactly one receive_complete per pend"
        );
        assert!(
            c.iter().all(|&l| l as usize == CHUNK),
            "completions must use the FULL saved indication length"
        );
    }

    /// Single-transition race, faithfully interleaved (finding 2): models a drain
    /// that runs BEFORE the callback's PENDING return path completes, exercising
    /// the real receive channel AND the real `RecvExec` seam. Sequence:
    ///
    /// 1. pre-fill just below the bound (sub-bound offers, left undrained);
    /// 2. the callback's locked transition for the crossing indication runs —
    ///    increment + decide-pend committed and the bytes PUBLISHED under the
    ///    lock (via `admit`), yielding the not-yet-acted-on pend outcome;
    /// 3. the drain INTERLEAVES HERE, before the callback returns PENDING:
    ///    `poll_data` observes the committed state, subtracts, and issues exactly
    ///    one `receive_complete` through the seam with the FULL SAVED length
    ///    (distinct from the drained length);
    /// 4. only then does the callback's return path resolve to PENDING.
    ///
    /// Asserts: no counter underflow, the pend is not lost (exactly one completion
    /// with the FULL saved length), and buffered accounting is exact.
    #[test]
    fn recv_state_transition_no_underflow_no_lost_pend() {
        use crate::{Admit, ReceiveEvent};

        const PRE: usize = 340 * 1024; // 3 pre-fill chunks = 1020 KiB (< bound)
        const RACE: usize = 200 * 1024; // the crossing indication (distinct size)

        let (mut ctx, _sctx, rctx) = stream_ctx_channel(0u64.try_into().unwrap());
        let rec = RecordingRecvExec::default();
        let completes = rec.completes.clone();
        let mut recv = H3RecvStream::with_exec(Box::new(rec), rctx);
        let mut cx = noop_context();

        // (1) Pre-fill to just below the bound WITHOUT draining.
        let pre = vec![0u8; PRE];
        for _ in 0..3 {
            assert!(feed_receive_result(&mut ctx, &pre, ReceiveFlags::NONE).is_ok());
        }
        assert_eq!(recv.rctx.recv_budget.buffered(), 3 * PRE);
        assert!(!recv.rctx.recv_budget.pend_outstanding());

        // (2) The callback's locked transition for the crossing indication:
        // exactly what the Receive arm does — `admit` commits the increment +
        // pend and PUBLISHES the bytes under the lock, returning the pend outcome
        // the callback has NOT yet acted on.
        let racing = Bytes::from(vec![0xEEu8; RACE]);
        let outcome = recv.rctx.recv_budget.admit(RACE, || {
            ctx.receive
                .as_ref()
                .expect("frontend live")
                .unbounded_send(ReceiveEvent::Data(racing))
                .is_ok()
        });
        assert!(matches!(outcome, Admit::Pend));
        assert!(recv.rctx.recv_budget.pend_outstanding());
        assert_eq!(
            recv.rctx.recv_budget.pending_indication_len() as usize,
            RACE
        );
        assert_eq!(recv.rctx.recv_budget.buffered(), 3 * PRE + RACE);

        // (3) INTERLEAVE: the drain runs NOW, before the callback's PENDING return
        // path completes. It observes the committed pend, subtracts the FIRST
        // buffered indication (PRE bytes), and — via the real `RecvExec` seam —
        // issues exactly one `receive_complete` with the FULL SAVED length (RACE),
        // NOT the drained length (PRE).
        match recv.poll_data(&mut cx) {
            Poll::Ready(Ok(Some(b))) => {
                assert_eq!(b.len(), PRE, "FIFO: the first buffered chunk drains first")
            }
            other => panic!("expected first buffered chunk, got {other:?}"),
        }
        assert_eq!(
            completes.lock().unwrap().as_slice(),
            &[RACE as u64],
            "pend not lost: one completion with the FULL saved length, not the drained length"
        );
        // No underflow, exact accounting: buffered decreased by exactly PRE.
        assert_eq!(recv.rctx.recv_budget.buffered(), 3 * PRE + RACE - PRE);
        assert!(!recv.rctx.recv_budget.pend_outstanding(), "window reopened");

        // (4) The callback's return path now resolves to PENDING (asserted via
        // `outcome` above); the early completion from (3) is absorbed — draining
        // the rest issues no further completion and never underflows.
        let mut drained = PRE;
        while let Poll::Ready(Ok(Some(b))) = recv.poll_data(&mut cx) {
            drained += b.len();
        }
        assert_eq!(
            drained,
            3 * PRE + RACE,
            "all bytes drained exactly, no loss"
        );
        assert_eq!(recv.rctx.recv_budget.buffered(), 0);
        assert_eq!(
            completes.lock().unwrap().len(),
            1,
            "exactly one completion total"
        );
    }

    /// Genuine concurrent race (finding 2): a writer thread runs the callback's
    /// `admit` publish-last transition while a reader thread drains and completes,
    /// hammering the single `RecvState` lock. Because the publish is the LAST step
    /// of `admit` (accounting first), a reader can never observe bytes before the
    /// increment lands — the `debug_assert!` in `on_drained` would fire on any
    /// underflow (this test would panic if the ordering regressed). Confirms the
    /// invariants under a real thread race: every published byte is eventually
    /// drained (buffered returns to exactly 0), no underflow, and pends are never
    /// stranded.
    #[test]
    fn drain_before_pend_return_is_absorbed() {
        use std::sync::mpsc;
        use std::thread;

        const ITERS: usize = 20_000;
        const CHUNK: usize = 4096;

        let budget = Arc::new(RecvBudget::default());
        let (tx, rx) = mpsc::channel::<usize>();

        let writer = {
            let budget = budget.clone();
            thread::spawn(move || {
                for _ in 0..ITERS {
                    // Exactly the callback's transition: the publish (channel send)
                    // is the LAST step, under the lock, after the increment.
                    budget.admit(CHUNK, || tx.send(CHUNK).is_ok());
                }
                drop(tx);
            })
        };
        let reader = {
            let budget = budget.clone();
            thread::spawn(move || {
                let mut completes = 0usize;
                let mut drained = 0usize;
                // Drain concurrently as bytes are published; a drain that raced
                // ahead of an increment would underflow (caught by debug_assert).
                while let Ok(len) = rx.recv() {
                    if budget.on_drained(len).is_some() {
                        completes += 1;
                    }
                    drained += len;
                }
                (completes, drained)
            })
        };

        writer.join().unwrap();
        let (completes, drained) = reader.join().unwrap();

        // Every published byte drained exactly; the counter never underflowed.
        assert_eq!(drained, ITERS * CHUNK);
        assert_eq!(
            budget.buffered(),
            0,
            "buffered returns to exactly 0 after the race"
        );
        // The stream is never left stranded PENDING with buffered below the bound.
        assert!(
            !budget.pend_outstanding(),
            "no pend left outstanding after the race"
        );
        println!("drain_before_pend_return_is_absorbed: completes={completes} drained={drained}");
    }

    /// Rollback-on-failed-publication (finding 1): if the frontend receiver is
    /// gone, the over-budget indication's locked transition PUBLISHES (send) and
    /// fails; the whole `RecvState` mutation (buffered, pend, saved length, peak)
    /// must be rolled back UNDER THE SAME LOCK, leaving accounting exactly as if
    /// the indication never arrived — and the callback must NOT return PENDING
    /// (a dropped frontend can never leave a stream stranded PENDING).
    #[test]
    fn dropped_frontend_rolls_back_recv_state() {
        let (mut ctx, _sctx, rctx) = stream_ctx_channel(0u64.try_into().unwrap());

        // A prior sub-bound indication succeeds, establishing non-zero baseline
        // accounting (buffered and peak) that MUST survive a later failed publish.
        let small = vec![0u8; 4096];
        assert!(feed_receive_result(&mut ctx, &small, ReceiveFlags::NONE).is_ok());
        let buffered_before = ctx.recv_budget.buffered();
        let peak_before = ctx.recv_budget.peak();
        assert_eq!(buffered_before, 4096);
        assert!(!ctx.recv_budget.pend_outstanding());

        // Drop the frontend receiver: every subsequent publication (send) fails.
        drop(rctx);

        // An over-bound indication whose publish fails: without rollback the buggy
        // path would leave buffered = 4096 + big, pend_outstanding = true and
        // pending_indication_len = big. With the fix, `admit` reverts all of it.
        let big = vec![0xABu8; MAX_RECV_BUFFER];
        let res = feed_receive_result(&mut ctx, &big, ReceiveFlags::NONE);
        assert!(
            res.is_ok(),
            "a dropped frontend must NOT strand the stream PENDING, got {res:?}"
        );
        assert_eq!(
            ctx.recv_budget.buffered(),
            buffered_before,
            "buffered rolled back exactly to the pre-indication value"
        );
        assert_eq!(
            ctx.recv_budget.peak(),
            peak_before,
            "peak rolled back exactly (no phantom growth)"
        );
        assert!(
            !ctx.recv_budget.pend_outstanding(),
            "no pend left outstanding after a failed publish"
        );
        assert_eq!(
            ctx.recv_budget.pending_indication_len(),
            0,
            "saved indication length rolled back"
        );
        // The dropped frontend is observed: the sender half is taken.
        assert!(
            ctx.receive.is_none(),
            "dropped frontend detaches the sender"
        );
    }

    /// SC-005 no regression: a promptly-draining consumer sees identical data,
    /// ordering, and terminal signalling, and `receive_complete` is NEVER issued
    /// for sub-bound traffic (it can never block a prompt consumer).
    #[test]
    fn prompt_drain_no_regression() {
        let (mut ctx, _sctx, rctx) = stream_ctx_channel(0u64.try_into().unwrap());
        let rec = RecordingRecvExec::default();
        let completes = rec.completes.clone();
        let mut recv = H3RecvStream::with_exec(Box::new(rec), rctx);
        let mut cx = noop_context();

        // Interleave feeds and drains with sub-bound data plus a trailing FIN.
        for i in 0..8u8 {
            assert!(feed_receive_result(&mut ctx, &[i, i, i], ReceiveFlags::NONE).is_ok());
            match recv.poll_data(&mut cx) {
                Poll::Ready(Ok(Some(b))) => assert_eq!(&b[..], &[i, i, i]),
                other => panic!("expected data {i}, got {other:?}"),
            }
        }
        // A FIN riding a final data indication: data first, then clean EOF.
        assert!(feed_receive_result(&mut ctx, &[9, 9], ReceiveFlags::FIN).is_ok());
        match recv.poll_data(&mut cx) {
            Poll::Ready(Ok(Some(b))) => assert_eq!(&b[..], &[9, 9]),
            other => panic!("expected trailing data, got {other:?}"),
        }
        assert!(matches!(recv.poll_data(&mut cx), Poll::Ready(Ok(None))));
        // Sticky clean EOF.
        assert!(matches!(recv.poll_data(&mut cx), Poll::Ready(Ok(None))));

        // No backpressure completion ever issued for a prompt, sub-bound consumer.
        assert!(
            completes.lock().unwrap().is_empty(),
            "receive_complete must never fire for sub-bound prompt draining"
        );
        assert_eq!(recv.rctx.recv_budget.buffered(), 0);
        assert!(!recv.rctx.recv_budget.pend_outstanding());
    }

    /// Per-stream independence: stalling ONE stream at its bound does not affect
    /// another stream on the same connection — each has its own 1 MiB budget.
    #[test]
    fn stalled_stream_does_not_affect_peer_stream() {
        // Stalled stream: fill to the bound.
        let (mut stalled_ctx, _s1, stalled_rctx) = stream_ctx_channel(0u64.try_into().unwrap());
        let rec1 = RecordingRecvExec::default();
        let stalled = H3RecvStream::with_exec(Box::new(rec1), stalled_rctx);
        let chunk = vec![0u8; MAX_RECV_BUFFER];
        assert!(
            is_pending(&feed_receive_result(
                &mut stalled_ctx,
                &chunk,
                ReceiveFlags::NONE
            )),
            "one full-bound indication pends the stalled stream"
        );
        assert!(stalled.rctx.recv_budget.pend_outstanding());

        // Independent stream: its own budget is untouched; sub-bound traffic is
        // accepted normally and never pends.
        let (mut other_ctx, _s2, other_rctx) = stream_ctx_channel(4u64.try_into().unwrap());
        let rec2 = RecordingRecvExec::default();
        let mut other = H3RecvStream::with_exec(Box::new(rec2), other_rctx);
        let mut cx = noop_context();
        for i in 0..4u8 {
            assert!(
                feed_receive_result(&mut other_ctx, &[i, i], ReceiveFlags::NONE).is_ok(),
                "peer stream must not be paused by the stalled stream"
            );
            match other.poll_data(&mut cx) {
                Poll::Ready(Ok(Some(b))) => assert_eq!(&b[..], &[i, i]),
                o => panic!("expected peer data {i}, got {o:?}"),
            }
        }
        assert!(!other.rctx.recv_budget.pend_outstanding());
        assert!(other.rctx.recv_budget.peak() < MAX_RECV_BUFFER);
        // The stalled stream is still pended (independent budgets).
        assert!(stalled.rctx.recv_budget.pend_outstanding());
    }

    /// Pre-size: a multi-buffer indication is copied into a single, summed-length
    /// allocation and concatenated in order (proving the pre-sum path).
    #[test]
    fn multi_buffer_indication_is_presized_and_concatenated() {
        let (mut ctx, _sctx, mut rrx) = stream_ctx_channel(0u64.try_into().unwrap());
        let a = [1u8, 2, 3];
        let b = [4u8, 5];
        let c = [6u8, 7, 8, 9];
        let bufs = [
            BufferRef::from(&a[..]),
            BufferRef::from(&b[..]),
            BufferRef::from(&c[..]),
        ];
        let mut total = (a.len() + b.len() + c.len()) as u64;
        let ev = StreamEvent::Receive {
            absolute_offset: 0,
            total_buffer_length: &mut total,
            buffers: &bufs,
            flags: ReceiveFlags::NONE,
        };
        assert!(stream_callback(&mut ctx, ev).is_ok());
        match poll(&mut rrx) {
            Poll::Ready(Ok(Some(bytes))) => {
                assert_eq!(&bytes[..], &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
                assert_eq!(bytes.len(), (a.len() + b.len() + c.len()));
            }
            other => panic!("expected concatenated data, got {other:?}"),
        }
        assert_eq!(rrx.recv_budget.buffered(), 9);
    }
}

/// Phase 5 (safe stream open & identity) unit tests: prove the pure stream-ID
/// validation, the accepted-stream borrow-before-own resolution (with its
/// connection-scoped failpoint seam), and the non-panicking classification of a
/// local stream's `StartComplete` outcome (including connection-caused
/// cancellation). Hermetic (no network): a live peer stream always has a valid
/// 62-bit ID and a real `StreamRef` cannot be forged, so the resolution logic is
/// exercised through its factored, testable seams. The real success path is
/// covered end-to-end by the loopback `basic_server_test`.
#[cfg(test)]
mod stream_open_identity {
    use h3::quic::{ConnectionErrorIncoming, StreamErrorIncoming};

    use crate::error::ConnectionTerminal;
    use crate::msquic::{Status, StatusCode};
    use crate::{
        accept_stream_id, classify_start_outcome, conn_ctx_channel, fail_fast_terminal,
        new_conn_terminal_slot, record_conn_terminal, stream_open_conn_error, validate_stream_id,
    };

    /// The largest valid QUIC stream ID (62-bit VarInt max).
    const MAX_VALID_ID: u64 = (1 << 62) - 1;

    #[test]
    fn validate_stream_id_boundaries() {
        // 0 and the 62-bit maximum are valid; anything larger is rejected.
        assert_eq!(validate_stream_id(0).unwrap().into_inner(), 0);
        assert_eq!(
            validate_stream_id(MAX_VALID_ID).unwrap().into_inner(),
            MAX_VALID_ID
        );
        assert!(validate_stream_id(MAX_VALID_ID + 1).is_err());
        assert!(validate_stream_id(u64::MAX).is_err());
    }

    #[test]
    fn accept_stream_id_success_takes_no_terminal() {
        let (mut ctx, crx) = conn_ctx_channel();
        let id = accept_stream_id(&mut ctx, || Ok(8)).expect("valid id accepted");
        assert_eq!(id.into_inner(), 8);
        // No terminal published; both acceptor senders remain open.
        assert!(fail_fast_terminal(&crx.terminal).is_none());
        assert!(ctx.uni.is_some() && ctx.bidi.is_some());
    }

    #[test]
    fn accept_stream_id_query_failure_publishes_internal_and_wakes_acceptors() {
        let (mut ctx, crx) = conn_ctx_channel();
        let status = Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR);
        // A native get_stream_id failure returns its Status from the callback...
        let err = accept_stream_id(&mut ctx, || Err(status.clone())).expect_err("query failed");
        assert_eq!(
            err.try_as_status_code().ok(),
            status.try_as_status_code().ok()
        );
        // ...publishes a fail-fast internal terminal the acceptors observe...
        match fail_fast_terminal(&crx.terminal) {
            Some(ConnectionErrorIncoming::InternalError(_)) => {}
            other => panic!("expected fail-fast InternalError, got {other:?}"),
        }
        // ...and drops both acceptor senders so parked acceptors are woken.
        assert!(ctx.uni.is_none() && ctx.bidi.is_none());
    }

    #[test]
    fn accept_stream_id_invalid_id_publishes_internal_and_returns_internal_status() {
        let (mut ctx, crx) = conn_ctx_channel();
        // A (synthetic) out-of-range ID fails h3 validation: internal terminal,
        // QUIC_STATUS_INTERNAL_ERROR returned so msquic closes the stream.
        let err = accept_stream_id(&mut ctx, || Ok(u64::MAX)).expect_err("invalid id rejected");
        assert_eq!(
            err.try_as_status_code().ok(),
            Some(StatusCode::QUIC_STATUS_INTERNAL_ERROR)
        );
        assert!(matches!(
            fail_fast_terminal(&crx.terminal),
            Some(ConnectionErrorIncoming::InternalError(_))
        ));
        assert!(ctx.uni.is_none() && ctx.bidi.is_none());
    }

    #[test]
    fn already_published_peer_terminal_wins_over_internal() {
        // An accepted-stream failure records Internal, but a peer application
        // close published first is preserved (first-writer-wins).
        let (mut ctx, crx) = conn_ctx_channel();
        record_conn_terminal(&ctx.terminal, ConnectionTerminal::PeerApplication(7));
        let _ = accept_stream_id(&mut ctx, || Ok(u64::MAX)).expect_err("invalid id rejected");
        // The winning terminal is the earlier peer close, not the internal fault.
        assert!(matches!(
            crate::observe_terminal(&crx.terminal),
            ConnectionErrorIncoming::ApplicationClose { error_code: 7 }
        ));
    }

    #[test]
    fn accepted_id_failpoint_query_fail_seam_rejects_then_consumes() {
        // Drive the exact seam the callback uses, but arm it through the
        // RECEIVER-side handle a live `Connection` frontend holds — proving the
        // failpoint atomic is shared with the callback's sender-side clone.
        let (mut ctx, crx) = conn_ctx_channel();
        crx.accepted_id_failpoint.arm_query_fail();
        // The callback consults its own (shared) sender-side handle.
        let fp = ctx.accepted_id_failpoint.clone();
        let err = accept_stream_id(&mut ctx, || fp.maybe_override(4)).expect_err("seam trips once");
        assert_eq!(
            err.try_as_status_code().ok(),
            Some(StatusCode::QUIC_STATUS_INTERNAL_ERROR)
        );
        assert!(matches!(
            fail_fast_terminal(&crx.terminal),
            Some(ConnectionErrorIncoming::InternalError(_))
        ));
        // The failpoint consumed itself: a fresh query now passes through.
        let fp2 = crx.accepted_id_failpoint.clone();
        assert_eq!(fp2.maybe_override(4).unwrap(), 4);
    }

    #[test]
    fn accepted_id_failpoint_invalid_seam_rejects() {
        let (mut ctx, crx) = conn_ctx_channel();
        // Arm through the receiver-side (frontend) handle; read via the sender.
        crx.accepted_id_failpoint.arm_invalid_id();
        let fp = ctx.accepted_id_failpoint.clone();
        // The seam yields an out-of-range ID, which fails validation downstream.
        let err = accept_stream_id(&mut ctx, || fp.maybe_override(4)).expect_err("invalid seam");
        assert_eq!(
            err.try_as_status_code().ok(),
            Some(StatusCode::QUIC_STATUS_INTERNAL_ERROR)
        );
        assert!(matches!(
            fail_fast_terminal(&crx.terminal),
            Some(ConnectionErrorIncoming::InternalError(_))
        ));
    }

    #[test]
    fn classify_start_outcome_valid_id() {
        let slot = new_conn_terminal_slot();
        let id = classify_start_outcome(Ok(12), &slot).expect("valid local id");
        assert_eq!(id.into_inner(), 12);
    }

    #[test]
    fn classify_start_outcome_invalid_local_id_is_internal() {
        let slot = new_conn_terminal_slot();
        match classify_start_outcome(Ok(u64::MAX), &slot) {
            Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(msg),
            }) => assert!(msg.contains("local stream ID is invalid"), "msg: {msg}"),
            other => panic!("expected nested InternalError, got {other:?}"),
        }
    }

    #[test]
    fn classify_start_outcome_failed_start_without_terminal_is_unknown() {
        let slot = new_conn_terminal_slot();
        let status = Status::new(StatusCode::QUIC_STATUS_ABORTED);
        match classify_start_outcome(Err(status), &slot) {
            Err(StreamErrorIncoming::Unknown(_)) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn classify_start_outcome_failed_start_with_terminal_is_connection_error() {
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(5));
        let status = Status::new(StatusCode::QUIC_STATUS_ABORTED);
        match classify_start_outcome(Err(status), &slot) {
            Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code: 5 },
            }) => {}
            other => panic!("expected connection ApplicationClose(5), got {other:?}"),
        }
    }

    #[test]
    fn stream_open_conn_error_without_reason_is_internal() {
        let slot = new_conn_terminal_slot();
        match stream_open_conn_error(&slot) {
            ConnectionErrorIncoming::InternalError(msg) => {
                assert!(
                    msg.contains("cancelled without a terminal reason"),
                    "msg: {msg}"
                );
            }
            other => panic!("expected InternalError, got {other:?}"),
        }
    }

    #[test]
    fn stream_open_conn_error_with_reason_converts_terminal() {
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::Timeout);
        assert!(matches!(
            stream_open_conn_error(&slot),
            ConnectionErrorIncoming::Timeout
        ));
    }

    #[test]
    fn classify_start_outcome_failed_start_commits_connection_cause() {
        // Phase 1 / SF-C: a delivered connection cause on a failed start COMMITS
        // (freezes) the shared slot, so a later refinement does not change what
        // other observers see (SC-003, stream-open delivery point).
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        let status = Status::new(StatusCode::QUIC_STATUS_ABORTED);
        match classify_start_outcome(Err(status), &slot) {
            Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code: 9 },
            }) => {}
            other => panic!("expected ApplicationClose{{9}}, got {other:?}"),
        }
        // Frozen on delivery: a later refinement is rejected.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }

    #[test]
    fn classify_start_outcome_failed_start_empty_slot_does_not_commit() {
        // Phase 1 / SF-C non-delivery: an empty slot surfaces `Unknown` WITHOUT
        // freezing, so a later real cause can still be recorded and delivered.
        let slot = new_conn_terminal_slot();
        let status = Status::new(StatusCode::QUIC_STATUS_ABORTED);
        match classify_start_outcome(Err(status), &slot) {
            Err(StreamErrorIncoming::Unknown(_)) => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }

    #[test]
    fn stream_open_conn_error_commits_connection_cause() {
        // Phase 1 / SF-C: the cancellation-branch delivery (`stream_open_conn_error`)
        // commits the shared slot when a cause is present; a later refinement is
        // rejected.
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            stream_open_conn_error(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }

    #[test]
    fn stream_open_conn_error_empty_slot_does_not_commit() {
        // Phase 1 / SF-C non-delivery: an empty slot maps to `InternalError` WITHOUT
        // freezing, leaving the slot refinable for a later genuine cause.
        let slot = new_conn_terminal_slot();
        match stream_open_conn_error(&slot) {
            ConnectionErrorIncoming::InternalError(_) => {}
            other => panic!("expected InternalError, got {other:?}"),
        }
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }

    // The REAL poll_open_inner cancellation branch (a dropped start sender mapping
    // to a connection error / nested internal error) is driven end-to-end through
    // the public poll_open_bidi / poll_open_send in
    // `downcall_clamp::poll_open_inner_start_cancellation_maps_through_real_function`.
    // The two helper unit tests above (`stream_open_conn_error_*`) independently
    // pin both arms of the mapping the branch relies on.

    /// The accepted-ID failpoint must be arm-able from a *live* `Connection`
    /// (what a Phase 8 loopback test holds) before any peer stream is accepted.
    /// A real, unstarted connection is opened (no network) and armed through the
    /// frontend `Connection`; the shared atomic the callback's accept path reads
    /// then trips exactly once — proving the seam is reachable end-to-end.
    #[test]
    fn live_connection_arms_accepted_id_failpoint() {
        use crate::msquic::{ConnectionEvent, ConnectionRef, RegistrationConfig};
        use crate::registration::RundownGuard;

        let reg = crate::Registration::new(&RegistrationConfig::default()).unwrap();
        // Open a real (unstarted) native connection, then attach the frontend —
        // exactly the ownership the listener's accept path produces.
        let inner =
            crate::msquic::Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| {
                Ok(())
            })
            .unwrap();
        let guard = RundownGuard::new(reg.state().clone());
        let conn = crate::Connection::attach(inner, guard);

        // Arm query-fail through the live `Connection`; the shared atomic the
        // accept path consults trips once then consumes itself.
        conn.arm_accepted_id_query_fail();
        assert!(
            conn.accepted_id_failpoint().maybe_override(4).is_err(),
            "armed query-fail must trip on the next accepted stream"
        );
        assert_eq!(
            conn.accepted_id_failpoint().maybe_override(4).unwrap(),
            4,
            "failpoint consumes itself after one trip"
        );

        // Arm invalid-id through the live `Connection`; the seam yields an
        // out-of-range ID that fails downstream validation.
        conn.arm_accepted_id_invalid();
        assert_eq!(
            conn.accepted_id_failpoint().maybe_override(4).unwrap(),
            u64::MAX,
            "armed invalid-id must yield an out-of-range ID"
        );

        // Close the connection (single native ConnectionClose) before the
        // registration is dropped.
        drop(conn);
    }
}

/// Phase 6 (Owned `SendBuffer` & command-executor seam) unit tests.
///
/// These prove the send seam mechanism WITHOUT a live connection: an injected
/// [`SendExec`] test double (`CountingExec`) shares the exact allocation/reclaim
/// contract as the production `StreamExecutor`, and the retained `client_context`
/// is replayed through the PRODUCTION [`stream_callback`] — the real reconstruct+
/// drop path — so the exactly-once ownership guarantee is exercised, not mocked.
/// The comprehensive `CountingExec` matrix + loopback ordering test are Phase 8.
#[cfg(test)]
mod send_seam {
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::{Arc, Mutex, PoisonError};

    use bytes::Bytes;
    use h3::proto::frame::Frame;
    use h3::quic::{SendStream, StreamErrorIncoming};

    use crate::error::{ConnectionTerminal, MAX_ADAPTER_SEND};
    use crate::msquic::{Status, StatusCode, StreamEvent};
    use crate::{
        ConnTerminalSlot, ConnectionErrorIncoming, H3SendStream, OversizedSend, SendBuffer,
        SendExec, SendStreamReceiveCtx, StreamSendCtx, new_conn_terminal_slot,
        record_conn_terminal, stream_callback, stream_ctx_channel, stream_ctx_channel_with_conn,
    };

    /// Shared, inspectable counters + `client_context` slots. A clone is kept by
    /// the test so every counter stays readable after the `CountingExec` is moved
    /// into `H3SendStream` behind `Box<dyn SendExec>`.
    #[derive(Clone, Debug, Default)]
    struct CountingHandle {
        sends: Arc<AtomicUsize>,
        gracefuls: Arc<AtomicUsize>,
        resets: Arc<AtomicUsize>,
        /// Every clamped code submitted to `submit_reset`, in order.
        reset_codes: Arc<Mutex<Vec<u64>>>,
        /// Total `Box<SendBuffer>` created (`Box::into_raw`).
        allocs: Arc<AtomicUsize>,
        /// In-exec reclamations (immediate-failure arm only).
        reclaims: Arc<AtomicUsize>,
        /// Raw pointers "native" holds; the test replays each through
        /// `stream_callback` (the production reclaim path).
        client_ctx: Arc<Mutex<VecDeque<usize>>>,
    }

    /// Test double for the send seam. Routes its owned-buffer submission through
    /// the SHARED production transaction (`crate::submit_owned_send`), so a
    /// scripted result models the real allocation and ownership handoff — the same
    /// code path `StreamExecutor::submit_send` uses — without a native stream.
    #[derive(Debug)]
    struct CountingExec {
        h: CountingHandle,
        script: VecDeque<Result<(), Status>>,
    }

    impl CountingExec {
        fn new(h: CountingHandle, script: VecDeque<Result<(), Status>>) -> Self {
            Self { h, script }
        }
    }

    impl SendExec for CountingExec {
        fn submit_send(&mut self, buf: SendBuffer) -> Result<(), Status> {
            self.h.sends.fetch_add(1, Relaxed);
            let scripted = self.script.pop_front().unwrap_or(Ok(()));
            let h = self.h.clone();
            // Route through the SHARED production ownership transaction
            // (`submit_owned_send`) so this test double exercises the exact
            // `Box::into_raw` / immediate-`Err` `Box::from_raw` handoff the real
            // `StreamExecutor` uses — a regression there cannot pass here.
            let res = crate::submit_owned_send(buf, |_buffers, cc| {
                h.allocs.fetch_add(1, Relaxed);
                match scripted {
                    // Native accepted ownership: retain the pointer so the test can
                    // replay the native SendComplete through stream_callback. The
                    // helper leaves the box outstanding (not dropped) on `Ok`.
                    Ok(()) => {
                        h.client_ctx
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push_back(cc as usize);
                        Ok(())
                    }
                    // Immediate failure: `submit_owned_send` reclaims the box here
                    // and now (its sole reclamation), exactly as StreamExecutor's
                    // immediate-`Err` arm does.
                    Err(status) => Err(status),
                }
            });
            if res.is_err() {
                // The shared helper performed the in-exec reclamation on `Err`.
                self.h.reclaims.fetch_add(1, Relaxed);
            }
            res
        }
        fn submit_graceful(&mut self) -> Result<(), Status> {
            self.h.gracefuls.fetch_add(1, Relaxed);
            Ok(())
        }
        fn submit_reset(&mut self, code: u64) -> Result<(), Status> {
            self.h.resets.fetch_add(1, Relaxed);
            self.h
                .reset_codes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(code);
            Ok(())
        }
    }

    /// Build a send-side context with NO live connection: an id-bearing set of
    /// channel halves. Returns the frontend `SendStreamReceiveCtx` plus the
    /// callback-owned `StreamSendCtx` (kept so the test can drive `stream_callback`).
    fn test_send_ctx() -> (SendStreamReceiveCtx, StreamSendCtx) {
        let id: h3::quic::StreamId = 0u64.try_into().expect("valid StreamId");
        let (ctx, sctx, _rctx) = stream_ctx_channel(id);
        (sctx, ctx)
    }

    /// Like [`test_send_ctx`] but sharing an explicit connection terminal slot, so
    /// an FR-013 test can refine the connection cause and observe how the send half
    /// resolves+freezes it.
    fn test_send_ctx_with_conn(
        conn_terminal: ConnTerminalSlot,
    ) -> (SendStreamReceiveCtx, StreamSendCtx) {
        let id: h3::quic::StreamId = 0u64.try_into().expect("valid StreamId");
        let (ctx, sctx, _rctx) = stream_ctx_channel_with_conn(id, conn_terminal);
        (sctx, ctx)
    }

    /// A connection-caused stream `ShutdownComplete` whose derived fallback is the
    /// PROVISIONAL `LocalClose` (by_app && !closed_remotely): the callback records
    /// it into the shared connection slot and publishes `Connection(LocalClose)`
    /// into the send-terminal slot.
    fn provisional_local_close(ctx: &mut StreamSendCtx) {
        stream_callback(
            ctx,
            StreamEvent::ShutdownComplete {
                connection_shutdown: true,
                app_close_in_progress: false,
                connection_shutdown_by_app: true,
                connection_closed_remotely: false,
                connection_error_code: 0,
                connection_close_status: Status::new(StatusCode::QUIC_STATUS_ABORTED),
            },
        )
        .unwrap();
    }

    #[test]
    fn f1_send_refinement_before_observation_surfaces_refined_cause() {
        // FR-013 (send side): a provisional connection fallback (LocalClose) is
        // refined on the SHARED slot to a specific peer application close BEFORE
        // the send half observes it at poll_ready → poll_ready reports the REFINED
        // ApplicationClose, not the stale LocalClose.
        let slot = new_conn_terminal_slot();
        let exec = Box::new(CountingExec::new(
            CountingHandle::default(),
            VecDeque::new(),
        ));
        let (sctx, mut ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        provisional_local_close(&mut ctx);
        // Refinement lands on the shared slot before the send half observes.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));

        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9),
            other => panic!("expected refined ApplicationClose{{9}}, got {other:?}"),
        }
        // Frozen after observation: a later refinement is rejected, and a
        // subsequent poll_finish reports the same refined cause.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(11));
        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9, "stable refined cause across observers"),
            other => panic!("expected stable ApplicationClose{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn f1_send_refinement_after_observation_is_rejected() {
        // FR-013 freeze-on-observation (send side): once poll_ready has OBSERVED
        // the connection winner (freezing the shared slot at the provisional
        // LocalClose), a later refinement is rejected and poll_finish keeps
        // reporting the observed LocalClose.
        let slot = new_conn_terminal_slot();
        let exec = Box::new(CountingExec::new(
            CountingHandle::default(),
            VecDeque::new(),
        ));
        let (sctx, mut ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        provisional_local_close(&mut ctx);
        // Observe FIRST at poll_ready — freezes the shared slot at LocalClose.
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::Undefined(e),
            })) => assert!(
                e.downcast_ref::<crate::LocalConnectionClose>().is_some(),
                "expected LocalConnectionClose, got {e:?}"
            ),
            other => panic!("expected LocalConnectionClose (Undefined), got {other:?}"),
        }
        // A refinement AFTER observation is rejected; poll_finish still reports the
        // observed LocalClose.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::Undefined(e),
            })) => assert!(
                e.downcast_ref::<crate::LocalConnectionClose>().is_some(),
                "post-observation refinement must be rejected, got {e:?}"
            ),
            other => panic!("expected frozen LocalConnectionClose, got {other:?}"),
        }
    }

    #[test]
    fn f1_reset_does_not_freeze_connection_winner_before_observation() {
        // FR-013 freeze-on-observation is exclusive to genuine CALLER observation
        // points (poll_data / poll_ready / poll_finish). `reset()` returns no
        // observable terminal to the caller, so it must NOT freeze the shared
        // connection slot: a provisional connection LocalClose, then reset(), then
        // a later peer refinement must still surface the REFINED specific cause at
        // the next caller observation (poll_ready) — not the stale LocalClose that
        // a premature freeze in reset() would have retained.
        let slot = new_conn_terminal_slot();
        let exec = Box::new(CountingExec::new(
            CountingHandle::default(),
            VecDeque::new(),
        ));
        let (sctx, mut ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        // Provisional connection LocalClose recorded on the shared slot and
        // published as the send winner.
        provisional_local_close(&mut ctx);
        // reset() consults the winner for bookkeeping (loses to the published
        // connection terminal, no native op) but must read WITHOUT freezing.
        SendStream::<Bytes>::reset(&mut s, 5);
        // The connection refines to a specific peer cause AFTER reset(); this must
        // still be honoured because reset() did not freeze the slot.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));

        // The next genuine caller observation surfaces the REFINED cause.
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9, "reset() must not freeze the refined cause"),
            other => panic!("expected refined ApplicationClose{{9}}, got {other:?}"),
        }
        // poll_ready has now frozen the slot at the refined cause: a later
        // refinement is rejected and poll_finish reports the same stable value.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(11));
        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9, "stable refined cause after observation"),
            other => panic!("expected stable ApplicationClose{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn send_data_wires_through_seam_and_signals_ready() {
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new())); // all-Ok script
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);

        // Valid WriteBuf<Bytes>: an h3 DATA frame, built via the real
        // From<Frame<B>> impl. send_data classifies it NonEmpty and submits.
        SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"hello world")))
            .unwrap();
        assert_eq!(h.sends.load(Relaxed), 1);
        assert_eq!(h.allocs.load(Relaxed), 1); // one Box<SendBuffer> created
        assert_eq!(h.reclaims.load(Relaxed), 0); // not the immediate-failure arm
        assert_eq!(h.client_ctx.lock().unwrap().len(), 1); // outstanding: "native" holds it

        // Replay the native SendComplete through the PRODUCTION callback path.
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: false,
                client_context: cc as *const c_void,
            },
        )
        .unwrap();

        // The callback reconstructed+dropped the box and enqueued exactly one
        // completion (`cancelled == false`); a second poll is empty. reclaims stays
        // 0 (reclaim happened in the callback, not the immediate-failure arm).
        assert_eq!(
            s.sctx.send.try_recv().unwrap(),
            crate::error::SendEvent::Complete { cancelled: false },
            "completion must report cancelled == false"
        );
        assert!(s.sctx.send.try_recv().is_err());
        assert_eq!(h.reclaims.load(Relaxed), 0);
    }

    #[test]
    fn send_buffer_reclaimed_exactly_once_via_callback() {
        // Concrete exactly-once proof: a drop-counted SendBuffer submitted (Ok) and
        // reclaimed by the production stream_callback increments its counter from 0
        // (outstanding) to exactly 1 (reclaimed once).
        let counter = Arc::new(AtomicUsize::new(0));
        let buf = SendBuffer::new_counted(Bytes::from_static(b"payload"), counter.clone());
        let h = CountingHandle::default();
        let mut exec = CountingExec::new(h.clone(), VecDeque::new());

        exec.submit_send(buf).unwrap();
        assert_eq!(h.allocs.load(Relaxed), 1);
        assert_eq!(
            counter.load(Relaxed),
            0,
            "outstanding buffer must not be dropped yet"
        );
        assert_eq!(h.client_ctx.lock().unwrap().len(), 1);

        let (_sctx, mut ctx) = test_send_ctx();
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: false,
                client_context: cc as *const c_void,
            },
        )
        .unwrap();

        assert_eq!(
            counter.load(Relaxed),
            1,
            "callback must reconstruct+drop the Box<SendBuffer> exactly once"
        );
        assert_eq!(h.reclaims.load(Relaxed), 0);
    }

    #[test]
    fn send_complete_null_client_context_surfaces_internal_error() {
        // F2 (see "Owned send buffer" in `docs/receive-and-send.md`): a null `SendComplete.client_context` breaks the
        // ownership invariant (every successful send attaches one
        // `Box<SendBuffer>`). The callback must NOT emit a normal `Complete`; it
        // publishes an internal send terminal and wakes the send half, so
        // `poll_ready` surfaces the internal error rather than a false readiness —
        // and it never panics or dereferences null.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);

        // Panic-free: a null-context SendComplete returns Ok across the FFI boundary.
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: false,
                client_context: std::ptr::null(),
            },
        )
        .expect("null-context SendComplete must be panic-free");

        // No normal `Complete` was enqueued; only a terminal wake, and the shared
        // send-terminal slot now holds the internal reason. `poll_ready` surfaces
        // the internal error, NOT `Ready(Ok(()))`.
        let mut cx = noop_cx();
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(msg),
            })) => assert!(
                msg.contains("SendComplete missing client context"),
                "unexpected internal message: {msg}"
            ),
            other => panic!("expected InternalError from null client_context, got {other:?}"),
        }
    }

    #[test]
    fn immediate_send_failure_reclaims_without_completion() {
        let h = CountingHandle::default();
        let mut script = VecDeque::new();
        script.push_back(Err(Status::from(StatusCode::QUIC_STATUS_INVALID_PARAMETER)));
        let exec = Box::new(CountingExec::new(h.clone(), script));
        let (sctx, _ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);

        // submit_send returns Err; send_data surfaces the error.
        assert!(
            SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"hello world")))
                .is_err()
        );

        // One allocation, one immediate in-exec reclamation, nothing outstanding,
        // and ZERO SendComplete callbacks (native took no ownership, emitted none).
        assert_eq!(h.sends.load(Relaxed), 1);
        assert_eq!(h.allocs.load(Relaxed), 1);
        assert_eq!(h.reclaims.load(Relaxed), 1);
        assert!(h.client_ctx.lock().unwrap().is_empty());
        assert!(s.sctx.send.try_recv().is_err()); // no Complete event ever enqueued
    }

    #[test]
    fn oversized_send_rejected_before_allocation() {
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, _ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);

        // A payload just past the ceiling: rejected before any copy or submission.
        let payload = Bytes::from(vec![0u8; MAX_ADAPTER_SEND as usize + 1]);
        let err = SendStream::<Bytes>::send_data(&mut s, Frame::Data(payload))
            .expect_err("oversized send must be rejected");

        match err {
            StreamErrorIncoming::Unknown(e) => {
                assert!(
                    e.downcast_ref::<OversizedSend>().is_some(),
                    "expected OversizedSend, got {e:?}"
                );
            }
            other => panic!("expected Unknown(OversizedSend), got {other:?}"),
        }
        // No allocation and no native submission occurred, and no send is in flight.
        assert_eq!(h.sends.load(Relaxed), 0);
        assert_eq!(h.allocs.load(Relaxed), 0);
        assert!(s.sctx.send.try_recv().is_err());
    }

    #[test]
    fn send_data_while_in_progress_returns_internal_error() {
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new())); // all-Ok
        let (sctx, _ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);

        // First send succeeds and marks a send in progress.
        SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"first"))).unwrap();
        assert_eq!(h.sends.load(Relaxed), 1);

        // Second send while the first is still outstanding is an internal error
        // (not a panic), and does not allocate or submit again.
        let err =
            SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"second")))
                .expect_err("send while in progress must error");
        match err {
            StreamErrorIncoming::ConnectionErrorIncoming { connection_error } => {
                assert!(
                    matches!(connection_error, ConnectionErrorIncoming::InternalError(_)),
                    "expected InternalError, got {connection_error:?}"
                );
            }
            other => panic!("expected ConnectionErrorIncoming::InternalError, got {other:?}"),
        }
        assert_eq!(h.sends.load(Relaxed), 1, "no second native submission");
        assert_eq!(h.allocs.load(Relaxed), 1, "no second allocation");
    }

    // ── Phase 7 frontend tests (reducer-driven send state machine) ──

    /// A leaked no-op waker gives a `'static` context reusable across polls.
    fn noop_cx() -> std::task::Context<'static> {
        let waker = Box::leak(Box::new(futures::task::noop_waker()));
        std::task::Context::from_waker(waker)
    }

    #[test]
    fn reset_issues_one_clamped_reset_stream_via_seam() {
        // reset submits exactly one native RESET_STREAM whose code is clamped to
        // the varint max at/below/above the ceiling; a second reset is a no-op.
        let max = (1u64 << 62) - 1;
        for (input, expected) in [(max, max), (1u64 << 62, max), (u64::MAX, max)] {
            let h = CountingHandle::default();
            let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
            let (sctx, _ctx) = test_send_ctx();
            let mut s = H3SendStream::with_exec(exec, sctx);

            // Infallible: returns `()`, never panics.
            SendStream::<Bytes>::reset(&mut s, input);
            assert_eq!(h.resets.load(Relaxed), 1, "exactly one native reset");
            assert_eq!(
                h.reset_codes.lock().unwrap().as_slice(),
                &[expected],
                "submitted reset code must be clamped (input {input:#x})"
            );
            assert!(
                !s.sctx.reducer.reset_submitting,
                "the reset reservation is cleared"
            );

            // A second reset finds the terminal already recorded: no second op.
            SendStream::<Bytes>::reset(&mut s, input);
            assert_eq!(h.resets.load(Relaxed), 1, "no second native reset");
        }
    }

    #[test]
    fn sf2_finish_completion_survives_intervening_poll_ready() {
        // SF-2: an intervening poll_ready after finish must NOT consume the queued
        // finish completion; a later poll_finish still observes it (no hang/loss).
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        // Start finish on an idle stream: one graceful shutdown, then Pending.
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Pending
        ));
        assert_eq!(h.gracefuls.load(Relaxed), 1);

        // The native completion is queued on the single ordered channel.
        stream_callback(
            &mut ctx,
            StreamEvent::SendShutdownComplete { graceful: true },
        )
        .unwrap();

        // SF-2 guard: poll_ready returns WITHOUT polling (consuming) the channel.
        assert!(
            matches!(
                SendStream::<Bytes>::poll_ready(&mut s, &mut cx),
                std::task::Poll::Ready(Err(_))
            ),
            "poll_ready after finish is a non-consuming error"
        );

        // The finish completion was preserved: poll_finish still completes cleanly.
        assert!(
            matches!(
                SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
                std::task::Poll::Ready(Ok(()))
            ),
            "queued finish completion must survive the intervening poll_ready"
        );
        assert_eq!(
            h.gracefuls.load(Relaxed),
            1,
            "graceful submitted exactly once"
        );
    }

    #[test]
    fn poll_finish_is_idempotent_one_graceful() {
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        // First poll_finish submits the graceful shutdown.
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Pending
        ));
        // Second poll_finish BEFORE completion must not re-submit.
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Pending
        ));
        assert_eq!(h.gracefuls.load(Relaxed), 1, "no second graceful shutdown");

        // Complete the finish; subsequent polls are absorbing Ok, still one graceful.
        stream_callback(
            &mut ctx,
            StreamEvent::SendShutdownComplete { graceful: true },
        )
        .unwrap();
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
        assert_eq!(h.gracefuls.load(Relaxed), 1);
    }

    #[test]
    fn peer_stop_sending_observable_without_send_in_flight() {
        // SC-003: a peer STOP_SENDING with no send in flight is observable at both
        // poll_ready and poll_finish as StreamTerminated{code}.
        for use_finish in [false, true] {
            let h = CountingHandle::default();
            let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
            let (sctx, mut ctx) = test_send_ctx();
            let mut s = H3SendStream::with_exec(exec, sctx);
            let mut cx = noop_cx();

            stream_callback(&mut ctx, StreamEvent::PeerReceiveAborted { error_code: 42 }).unwrap();

            let got = if use_finish {
                SendStream::<Bytes>::poll_finish(&mut s, &mut cx)
            } else {
                SendStream::<Bytes>::poll_ready(&mut s, &mut cx)
            };
            match got {
                std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated {
                    error_code,
                })) => assert_eq!(error_code, 42),
                other => panic!("expected StreamTerminated{{42}}, got {other:?}"),
            }
            assert_eq!(h.sends.load(Relaxed), 0, "no send was ever issued");
        }
    }

    #[test]
    fn peer_stop_sending_observed_with_send_outstanding() {
        // Item 1 (Phase 8): the DETERMINISTIC proof of "peer STOP_SENDING with a
        // send genuinely outstanding". Over pure loopback msquic copies a buffered
        // send and completes it synchronously, so a send cannot be *held*
        // observably outstanding across the public API (see the conformance
        // module's idle-only note); that condition is proven here at the seam
        // instead. The `CountingExec` retains the "native"-owned `Box<SendBuffer>`
        // exactly as `StreamExecutor` does, so the send is provably still
        // outstanding (not yet reclaimed, `send_inprogress` set) at the moment
        // STOP_SENDING is observed — then surfaced at `poll_ready` as the sticky
        // `StreamTerminated{code}`, and the outstanding box reclaimed exactly once
        // through the production `stream_callback`.
        const STOP_CODE: u64 = 0x6364;
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new())); // all-Ok
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        // 1. Issue a data send: the seam accepts ownership and RETAINS the box, so
        //    the send is now genuinely outstanding (nothing reclaimed yet).
        SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"more-data")))
            .unwrap();
        assert_eq!(h.sends.load(Relaxed), 1);
        assert_eq!(
            h.client_ctx.lock().unwrap().len(),
            1,
            "the send is outstanding: native holds the retained buffer"
        );
        assert_eq!(h.reclaims.load(Relaxed), 0, "nothing reclaimed yet");
        assert!(
            s.sctx.reducer.send_inprogress,
            "the reducer marks the send in progress"
        );

        // 2. Peer STOP_SENDING arrives WHILE that send is still outstanding: the
        //    retained box is provably NOT yet reclaimed, so this is the true
        //    in-flight condition the loopback test could not establish.
        stream_callback(
            &mut ctx,
            StreamEvent::PeerReceiveAborted {
                error_code: STOP_CODE,
            },
        )
        .unwrap();
        assert_eq!(
            h.client_ctx.lock().unwrap().len(),
            1,
            "STOP_SENDING observed with the send still outstanding (not completed)"
        );
        assert!(
            s.sctx.reducer.send_inprogress,
            "the send is still in progress when the stop is published"
        );

        // 3. The following poll_ready surfaces the sticky terminal even though the
        //    outstanding send has not completed, preserving the exact stop code.
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(
                    error_code, STOP_CODE,
                    "exact peer stop code preserved (in flight)"
                );
            }
            other => panic!("expected StreamTerminated{{{STOP_CODE:#x}}}, got {other:?}"),
        }

        // 4. The native (cancelled) SendComplete for that outstanding send still
        //    arrives; the production callback reclaims the retained box exactly
        //    once, mirroring real teardown and leaving nothing outstanding.
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: true,
                client_context: cc as *const c_void,
            },
        )
        .unwrap();
        assert_eq!(
            h.reclaims.load(Relaxed),
            0,
            "reclaim happens in the callback, not the immediate-failure arm"
        );
        assert!(
            h.client_ctx.lock().unwrap().is_empty(),
            "no send left outstanding after the cancelled completion"
        );
    }

    #[test]
    fn reset_loses_to_published_peer_terminal_no_native_op() {
        // Local reset racing peer STOP_SENDING, terminal-first order: the
        // callback-published terminal wins, no native reset is issued.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        stream_callback(&mut ctx, StreamEvent::PeerReceiveAborted { error_code: 9 }).unwrap();
        SendStream::<Bytes>::reset(&mut s, 5); // infallible; sees the terminal
        assert_eq!(
            h.resets.load(Relaxed),
            0,
            "no native reset behind a terminal"
        );
        assert!(
            !s.sctx.reducer.reset_submitting,
            "no reservation left behind"
        );

        // The stable winner is the peer terminal, not the local reset code.
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(error_code, 9)
            }
            other => panic!("expected StreamTerminated{{9}}, got {other:?}"),
        }
        SendStream::<Bytes>::reset(&mut s, 5);
        assert_eq!(h.resets.load(Relaxed), 0, "still no native reset");
    }

    #[test]
    fn reset_loses_to_published_connection_terminal_no_native_op() {
        // Local reset racing a connection shutdown, terminal-first order.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        stream_callback(
            &mut ctx,
            StreamEvent::ShutdownComplete {
                connection_shutdown: true,
                app_close_in_progress: false,
                connection_shutdown_by_app: true,
                connection_closed_remotely: true,
                connection_error_code: 7,
                connection_close_status: Status::new(StatusCode::QUIC_STATUS_ABORTED),
            },
        )
        .unwrap();
        SendStream::<Bytes>::reset(&mut s, 5);
        assert_eq!(
            h.resets.load(Relaxed),
            0,
            "no native reset behind a terminal"
        );
        assert!(!s.sctx.reducer.reset_submitting);

        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 7),
            other => panic!("expected connection ApplicationClose{{7}}, got {other:?}"),
        }
    }

    #[test]
    fn reset_first_stays_stable_winner_with_single_native_reset() {
        // The other order: local reset genuinely completes first (empty slot), so
        // it issues exactly one native reset and remains the stable winner even
        // when a peer terminal is published afterwards (first-writer, non-provisional).
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        SendStream::<Bytes>::reset(&mut s, 5);
        assert_eq!(h.resets.load(Relaxed), 1);
        assert!(
            !s.sctx.reducer.reset_submitting,
            "the reset reservation is cleared after ResetSubmitted"
        );

        // A later peer STOP_SENDING cannot overwrite the specific LocalReset winner
        // nor trigger a second native reset.
        stream_callback(&mut ctx, StreamEvent::PeerReceiveAborted { error_code: 9 }).unwrap();
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::Unknown(_))) => {}
            other => panic!("expected Unknown(LocalStreamReset), got {other:?}"),
        }
        assert_eq!(h.resets.load(Relaxed), 1, "still exactly one native reset");
    }

    #[test]
    fn reset_first_stays_stable_winner_over_connection_shutdown() {
        // Item 3(b): the reset-first vs CONNECTION-shutdown race (connection variant
        // of the peer race above). Local reset completes first (empty slot), so it
        // issues exactly one native reset and remains the stable winner even when a
        // connection shutdown is published afterwards; the reservation is cleared.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        SendStream::<Bytes>::reset(&mut s, 5);
        assert_eq!(h.resets.load(Relaxed), 1);
        assert!(
            !s.sctx.reducer.reset_submitting,
            "the reset reservation is cleared after ResetSubmitted"
        );

        // A later connection shutdown cannot overwrite the specific LocalReset
        // winner nor trigger a second native reset.
        stream_callback(
            &mut ctx,
            StreamEvent::ShutdownComplete {
                connection_shutdown: true,
                app_close_in_progress: false,
                connection_shutdown_by_app: true,
                connection_closed_remotely: true,
                connection_error_code: 7,
                connection_close_status: Status::new(StatusCode::QUIC_STATUS_ABORTED),
            },
        )
        .unwrap();
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::Unknown(_))) => {}
            other => panic!("expected Unknown(LocalStreamReset), got {other:?}"),
        }
        assert_eq!(h.resets.load(Relaxed), 1, "still exactly one native reset");
    }

    /// Submit one non-empty send through the seam and drive it in-progress, then
    /// replay a *cancelled* native `SendComplete` through the PRODUCTION callback
    /// (reconstruct+drop the retained `client_context`). Leaves the cancelled
    /// completion queued on the single ordered channel, an outstanding-send seam.
    fn submit_then_cancel(h: &CountingHandle, s: &mut H3SendStream, ctx: &mut StreamSendCtx) {
        SendStream::<Bytes>::send_data(s, Frame::Data(Bytes::from_static(b"payload"))).unwrap();
        assert_eq!(h.sends.load(Relaxed), 1, "one native submission");
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            ctx,
            StreamEvent::SendComplete {
                cancelled: true,
                client_context: cc as *const std::ffi::c_void,
            },
        )
        .unwrap();
    }

    #[test]
    fn mf2_cancellation_first_refines_to_peer_stop_before_observation() {
        // Item 3(a): cancellation-FIRST order with an outstanding send. The cancelled
        // completion is processed BEFORE the paired peer cause; the provisional stays
        // UNOBSERVED (Pending), then the peer cause published before observation is the
        // caller's FIRST observed terminal, and it is stable afterwards.
        // FIRST observation is the refined specific cause, never Failed(ABORTED).
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        submit_then_cancel(&h, &mut s, &mut ctx);

        // Process the cancelled completion with NO cause yet: unobserved, Pending.
        assert!(
            matches!(
                SendStream::<Bytes>::poll_ready(&mut s, &mut cx),
                std::task::Poll::Pending
            ),
            "a no-cause cancellation must NOT freeze/return a provisional abort"
        );

        // The paired peer cause is published before the caller observes anything.
        stream_callback(&mut ctx, StreamEvent::PeerReceiveAborted { error_code: 9 }).unwrap();

        // FIRST observation is the refined specific cause, never Failed(ABORTED).
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(error_code, 9)
            }
            other => panic!("expected refined StreamTerminated{{9}}, got {other:?}"),
        }
        // Stable after observation (frozen): the same class every later poll.
        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(error_code, 9)
            }
            other => panic!("expected stable StreamTerminated{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn mf2_cancellation_first_refines_to_connection_before_observation() {
        // Item 3(a), connection variant: cancellation-FIRST, then a connection
        // shutdown refines the unobserved provisional to the connection cause.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        submit_then_cancel(&h, &mut s, &mut ctx);

        assert!(
            matches!(
                SendStream::<Bytes>::poll_ready(&mut s, &mut cx),
                std::task::Poll::Pending
            ),
            "unobserved provisional cancellation waits for the closure point"
        );

        // Connection shutdown publishes Connection(reason) then closes the channel.
        stream_callback(
            &mut ctx,
            StreamEvent::ShutdownComplete {
                connection_shutdown: true,
                app_close_in_progress: false,
                connection_shutdown_by_app: true,
                connection_closed_remotely: true,
                connection_error_code: 7,
                connection_close_status: Status::new(StatusCode::QUIC_STATUS_ABORTED),
            },
        )
        .unwrap();

        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 7),
            other => panic!("expected refined connection ApplicationClose{{7}}, got {other:?}"),
        }
        // Stable after observation.
        assert!(matches!(
            SendStream::<Bytes>::poll_ready(&mut s, &mut cx),
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming { .. }))
        ));
    }

    #[test]
    fn mf2_terminal_first_peer_stop_with_outstanding_send() {
        // The other callback order (terminal-FIRST) with an outstanding send: the
        // peer cause is published before the cancelled completion is processed, so it
        // is surfaced directly — the provisional is never even synthesized.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        // Peer STOP_SENDING (publishes Stopped + TerminalWake) BEFORE the cancelled
        // completion, mirroring MsQuic's documented PeerReceiveAborted ordering.
        SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"payload")))
            .unwrap();
        stream_callback(&mut ctx, StreamEvent::PeerReceiveAborted { error_code: 4 }).unwrap();
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: true,
                client_context: cc as *const std::ffi::c_void,
            },
        )
        .unwrap();

        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(error_code, 4)
            }
            other => panic!("expected StreamTerminated{{4}}, got {other:?}"),
        }
    }

    #[test]
    fn mf2_cancellation_only_closure_yields_authoritative_abort() {
        // The truly no-cause case: a cancelled completion followed by the channel
        // closing with NO published cause reaches the closure point and yields an
        // authoritative Failed(ABORTED) (Unknown), never a hang.
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx();
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        submit_then_cancel(&h, &mut s, &mut ctx);
        assert!(matches!(
            SendStream::<Bytes>::poll_ready(&mut s, &mut cx),
            std::task::Poll::Pending
        ));

        // Close the channel with no cause published (the closure point / stream drop).
        ctx.send.take();
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::Unknown(_))) => {}
            other => panic!("expected authoritative Unknown(ABORTED) at closure, got {other:?}"),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Phase 8: the FULL deterministic `CountingExec` send-seam matrix.
    //
    // Classification: SEAM tests (NOT loopback). These own the comprehensive
    // outstanding-send retain/reclaim and immediate-failure reclaim matrix the
    // design lists (Phase 6 proved the minimal seam; Phase 8 owns the matrix).
    // Every reclamation is driven through the PRODUCTION `stream_callback` — the
    // exact `NonNull` check → reconstruct+drop `Box<SendBuffer>` → one
    // `SendEvent::Complete` path — so the exactly-once ownership contract is
    // exercised, not mocked. Buffers carry a drop counter (`new_counted`) so the
    // reclamation COUNT is asserted concretely (this is the count that a real
    // loopback send cannot expose; see `docs/testing.md`,
    // "Native-test mechanisms").
    //
    // These are self-checking and deterministic: if the outstanding allocation
    // cannot be read as expected the test FAILS with a setup-specific message —
    // it is never skipped or `#[ignore]`d (Rust has no "inconclusive" outcome).
    // ─────────────────────────────────────────────────────────────────────────

    /// SEAM. The canonical `accepted_send_reclaims_via_callback_exactly_once`:
    /// proves, in order, (1) the deterministic blocking condition was established
    /// (buffer retained without a `SendComplete`); (2) exactly one adapter
    /// allocation is outstanding after submit returns; (3) feeding the completion
    /// through `stream_callback` causes exactly one callback reclamation and one
    /// `Complete`; (4) the count is back to zero (exactly once).
    #[test]
    fn accepted_send_reclaims_via_callback_exactly_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let buf = SendBuffer::new_counted(Bytes::from_static(b"outstanding"), counter.clone());
        let h = CountingHandle::default();
        let mut exec = CountingExec::new(h.clone(), VecDeque::new()); // all-Ok script

        exec.submit_send(buf).unwrap();

        // (1)+(2): blocking condition established — one alloc, one outstanding
        // pointer retained by "native", zero in-exec reclaims, buffer NOT dropped.
        assert_eq!(h.allocs.load(Relaxed), 1, "setup: exactly one allocation");
        assert_eq!(
            h.client_ctx.lock().unwrap().len(),
            1,
            "setup: exactly one outstanding pointer retained (no SendComplete yet)"
        );
        assert_eq!(h.reclaims.load(Relaxed), 0, "setup: no in-exec reclaim");
        assert_eq!(
            counter.load(Relaxed),
            0,
            "setup: outstanding buffer must not be dropped yet"
        );

        // (3): feed the retained client_context through the PRODUCTION callback.
        let (mut sctx, mut ctx) = test_send_ctx();
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: false,
                client_context: cc as *const c_void,
            },
        )
        .unwrap();

        // (4): reclaimed exactly once; exactly one Complete; a second read empty.
        assert_eq!(
            counter.load(Relaxed),
            1,
            "callback must reconstruct+drop the Box<SendBuffer> exactly once"
        );
        assert_eq!(
            sctx.send.try_recv().unwrap(),
            crate::error::SendEvent::Complete { cancelled: false },
            "exactly one completion, reporting cancelled == false"
        );
        assert!(
            sctx.send.try_recv().is_err(),
            "no second completion (reclaimed exactly once)"
        );
    }

    /// SEAM. Multiple concurrently-outstanding accepted sends: N drop-counted
    /// buffers are submitted (all Ok) and each is retained by "native"; feeding
    /// each retained `client_context` back through `stream_callback` reclaims that
    /// buffer exactly once and enqueues exactly one `Complete` — N reclamations,
    /// N completions, no double-free, no leak.
    #[test]
    fn multiple_outstanding_sends_each_reclaimed_exactly_once() {
        const N: usize = 5;
        let h = CountingHandle::default();
        let mut exec = CountingExec::new(h.clone(), VecDeque::new());
        let counters: Vec<_> = (0..N).map(|_| Arc::new(AtomicUsize::new(0))).collect();

        for c in &counters {
            let buf = SendBuffer::new_counted(Bytes::from_static(b"payload"), c.clone());
            exec.submit_send(buf).unwrap();
        }

        // All N outstanding: N allocations, N retained pointers, zero reclaimed.
        assert_eq!(h.allocs.load(Relaxed), N);
        assert_eq!(h.client_ctx.lock().unwrap().len(), N);
        assert_eq!(h.reclaims.load(Relaxed), 0);
        for c in &counters {
            assert_eq!(
                c.load(Relaxed),
                0,
                "no buffer reclaimed before its callback"
            );
        }

        // Feed each retained pointer through the production callback path.
        let (mut sctx, mut ctx) = test_send_ctx();
        let pending: Vec<usize> = h.client_ctx.lock().unwrap().drain(..).collect();
        for cc in pending {
            stream_callback(
                &mut ctx,
                StreamEvent::SendComplete {
                    cancelled: false,
                    client_context: cc as *const c_void,
                },
            )
            .unwrap();
        }

        // Each buffer reclaimed exactly once; exactly N completions delivered.
        for c in &counters {
            assert_eq!(c.load(Relaxed), 1, "each buffer reclaimed exactly once");
        }
        let mut completes = 0;
        while sctx.send.try_recv().is_ok() {
            completes += 1;
        }
        assert_eq!(completes, N, "exactly one Complete per outstanding send");
    }

    /// SEAM. Immediate-failure reclaim asserted with a drop counter: on a scripted
    /// `Err`, `submit_send` reclaims the box in place (mirroring `StreamExecutor`),
    /// so `allocs == 1`, `reclaims == 1`, the counter reads exactly 1, nothing is
    /// retained, and ZERO `SendComplete` callbacks fire (native took no ownership).
    #[test]
    fn immediate_failure_reclaims_in_place_counted() {
        let counter = Arc::new(AtomicUsize::new(0));
        let buf = SendBuffer::new_counted(Bytes::from_static(b"doomed"), counter.clone());
        let h = CountingHandle::default();
        let mut script = VecDeque::new();
        script.push_back(Err(Status::from(StatusCode::QUIC_STATUS_INVALID_PARAMETER)));
        let mut exec = CountingExec::new(h.clone(), script);

        exec.submit_send(buf)
            .expect_err("scripted immediate failure");

        assert_eq!(h.allocs.load(Relaxed), 1, "one allocation before failing");
        assert_eq!(
            h.reclaims.load(Relaxed),
            1,
            "reclaimed in place, exactly once"
        );
        assert_eq!(counter.load(Relaxed), 1, "the box was dropped exactly once");
        assert!(
            h.client_ctx.lock().unwrap().is_empty(),
            "nothing retained on immediate failure"
        );
    }

    /// SEAM. Mixed Ok/Err script: only accepted (Ok) sends are retained
    /// outstanding; the rejected (Err) send is reclaimed in place. Then draining
    /// the retained pointers through the callback reclaims each accepted buffer
    /// exactly once — proving the retain-vs-reclaim bookkeeping does not conflate
    /// the two ownership handoffs.
    #[test]
    fn mixed_ok_err_script_retains_only_accepted_sends() {
        let h = CountingHandle::default();
        let mut script = VecDeque::new();
        script.push_back(Ok(()));
        script.push_back(Err(Status::from(StatusCode::QUIC_STATUS_INVALID_PARAMETER)));
        script.push_back(Ok(()));
        let mut exec = CountingExec::new(h.clone(), script);

        let c_ok1 = Arc::new(AtomicUsize::new(0));
        let c_err = Arc::new(AtomicUsize::new(0));
        let c_ok2 = Arc::new(AtomicUsize::new(0));
        exec.submit_send(SendBuffer::new_counted(
            Bytes::from_static(b"a"),
            c_ok1.clone(),
        ))
        .unwrap();
        exec.submit_send(SendBuffer::new_counted(
            Bytes::from_static(b"b"),
            c_err.clone(),
        ))
        .expect_err("scripted Err");
        exec.submit_send(SendBuffer::new_counted(
            Bytes::from_static(b"c"),
            c_ok2.clone(),
        ))
        .unwrap();

        // Three allocations; the Err was reclaimed in place; two retained.
        assert_eq!(h.allocs.load(Relaxed), 3);
        assert_eq!(h.reclaims.load(Relaxed), 1);
        assert_eq!(h.client_ctx.lock().unwrap().len(), 2);
        assert_eq!(c_err.load(Relaxed), 1, "rejected buffer reclaimed in place");
        assert_eq!(c_ok1.load(Relaxed), 0, "accepted buffer still outstanding");
        assert_eq!(c_ok2.load(Relaxed), 0, "accepted buffer still outstanding");

        // Drain the two retained pointers through the production callback.
        let (_sctx, mut ctx) = test_send_ctx();
        let pending: Vec<usize> = h.client_ctx.lock().unwrap().drain(..).collect();
        for cc in pending {
            stream_callback(
                &mut ctx,
                StreamEvent::SendComplete {
                    cancelled: false,
                    client_context: cc as *const c_void,
                },
            )
            .unwrap();
        }
        assert_eq!(
            c_ok1.load(Relaxed),
            1,
            "first accepted reclaimed exactly once"
        );
        assert_eq!(
            c_ok2.load(Relaxed),
            1,
            "second accepted reclaimed exactly once"
        );
    }

    /// SEAM. A cancelled `SendComplete` (native cancel flag set) still reclaims
    /// the outstanding buffer exactly once through the production callback, and
    /// enqueues a `Complete { cancelled: true }` — the reclamation is independent
    /// of the cancel flag (ownership is returned either way).
    #[test]
    fn cancelled_send_complete_reclaims_buffer_exactly_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let buf = SendBuffer::new_counted(Bytes::from_static(b"cancelled"), counter.clone());
        let h = CountingHandle::default();
        let mut exec = CountingExec::new(h.clone(), VecDeque::new());
        exec.submit_send(buf).unwrap();
        assert_eq!(counter.load(Relaxed), 0);

        let (_sctx, mut ctx) = test_send_ctx();
        let cc = h.client_ctx.lock().unwrap().pop_front().unwrap();
        stream_callback(
            &mut ctx,
            StreamEvent::SendComplete {
                cancelled: true,
                client_context: cc as *const c_void,
            },
        )
        .unwrap();

        assert_eq!(
            counter.load(Relaxed),
            1,
            "a cancelled completion still reclaims the box exactly once"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Phase 1: terminal-cause commit-on-delivery (MF-1, MF-2, SC-003 send side).
    // Dual-callback-ordering seam tests: the connection cause is injected DURING
    // the executor downcall (via `InjectingExec`) so the immediate native error and
    // the connection-cause recording interleave exactly as under concurrent
    // callbacks, exercising the `resolve_terminal` (non-freezing) /
    // `commit_send_winner` (freeze-on-delivery) split rather than a pre-populated
    // short-circuit.
    // ─────────────────────────────────────────────────────────────────────────

    /// Send-side seam that records a connection terminal into the SHARED slot
    /// *during* the executor downcall and then returns an immediate native error —
    /// modelling the MF-1 window in which the connection callback records the cause
    /// concurrently with an immediate send/finish/reset failure. `submit_send`
    /// routes through the shared owned-buffer transaction so the buffer is reclaimed
    /// exactly as the immediate-`Err` arm requires.
    #[derive(Debug)]
    struct InjectingExec {
        slot: ConnTerminalSlot,
        cause: ConnectionTerminal,
        status: Status,
    }

    impl InjectingExec {
        fn new(slot: ConnTerminalSlot, cause: ConnectionTerminal) -> Self {
            Self {
                slot,
                cause,
                status: Status::from(StatusCode::QUIC_STATUS_INVALID_PARAMETER),
            }
        }
    }

    impl SendExec for InjectingExec {
        fn submit_send(&mut self, buf: SendBuffer) -> Result<(), Status> {
            let status = self.status.clone();
            let slot = self.slot.clone();
            let cause = self.cause.clone();
            crate::submit_owned_send(buf, move |_buffers, _cc| {
                record_conn_terminal(&slot, cause);
                Err(status)
            })
        }
        fn submit_graceful(&mut self) -> Result<(), Status> {
            record_conn_terminal(&self.slot, self.cause.clone());
            Err(self.status.clone())
        }
        fn submit_reset(&mut self, _code: u64) -> Result<(), Status> {
            record_conn_terminal(&self.slot, self.cause.clone());
            Err(self.status.clone())
        }
    }

    #[test]
    fn mf1_send_data_immediate_error_delivers_injected_connection_cause() {
        // SC-001 / MF-1: the shared slot is EMPTY until the executor downcall
        // records the cause, so the send slot never becomes `Connection` before the
        // immediate `Err` — exactly the concurrent-callback race. The send_data
        // return itself must deliver the specific connection cause (code 9), not
        // `Unknown(status)`.
        let slot = new_conn_terminal_slot();
        let exec = Box::new(InjectingExec::new(
            slot.clone(),
            ConnectionTerminal::PeerApplication(9),
        ));
        let (sctx, _ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);

        match SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"data"))) {
            Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            }) => assert_eq!(
                error_code, 9,
                "specific connection cause, not Unknown(status)"
            ),
            other => panic!("expected ApplicationClose{{9}}, got {other:?}"),
        }
        // The send slot ends holding Connection(PeerApplication(9)), not Failed.
        match crate::load_winner(&s.sctx.send_terminal) {
            Some(crate::error::SendTerminal::Connection(ConnectionTerminal::PeerApplication(
                9,
            ))) => {}
            other => panic!("expected send slot Connection(PeerApplication(9)), got {other:?}"),
        }
    }

    #[test]
    fn mf1_poll_finish_immediate_error_delivers_injected_connection_cause() {
        // SC-001 / MF-1 for the graceful-submit downcall: an idle poll_finish
        // submits the graceful shutdown, which injects the cause and fails
        // immediately; the poll return delivers the specific connection cause.
        let slot = new_conn_terminal_slot();
        let exec = Box::new(InjectingExec::new(
            slot.clone(),
            ConnectionTerminal::PeerApplication(9),
        ));
        let (sctx, _ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(error_code, 9),
            other => panic!("expected ApplicationClose{{9}}, got {other:?}"),
        }
        match crate::load_winner(&s.sctx.send_terminal) {
            Some(crate::error::SendTerminal::Connection(ConnectionTerminal::PeerApplication(
                9,
            ))) => {}
            other => panic!("expected send slot Connection(PeerApplication(9)), got {other:?}"),
        }
    }

    #[test]
    fn mf1_reset_injected_cause_surfaced_by_subsequent_poll() {
        // SC-001 / MF-1 for reset: reset() returns NOTHING to h3, so the injected
        // cause cannot be read from the reset() return; it is recorded during the
        // reset downcall and published as the send winner WITHOUT freezing. A
        // subsequent poll_finish surfaces the specific connection cause (code 9),
        // confirming the injected connection cause won the resolve after reset.
        let slot = new_conn_terminal_slot();
        let exec = Box::new(InjectingExec::new(
            slot.clone(),
            ConnectionTerminal::PeerApplication(9),
        ));
        let (sctx, _ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        SendStream::<Bytes>::reset(&mut s, 5);
        match SendStream::<Bytes>::poll_finish(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(
                error_code, 9,
                "reset did not freeze; the cause is delivered later"
            ),
            other => panic!("expected ApplicationClose{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn resolve_prefers_recorded_connection_over_provisional_marker() {
        // Provisional-winner regression (resolve precedence): drive the send slot
        // to the provisional `ProvisionalAbort` marker (a cancelled SendComplete
        // with no published cause), then record a connection cause on the shared
        // slot. `resolve_terminal` must peek the connection slot as a fallback for
        // the provisional winner, so a subsequent poll delivers the specific
        // connection cause (code 9) instead of masking it with `ProvisionalAbort`.
        let slot = new_conn_terminal_slot();
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        submit_then_cancel(&h, &mut s, &mut ctx);
        // Process the cancelled completion with NO cause: the provisional marker is
        // retained (unobserved), poll is Pending.
        assert!(matches!(
            SendStream::<Bytes>::poll_ready(&mut s, &mut cx),
            std::task::Poll::Pending
        ));
        assert!(
            matches!(
                crate::load_winner(&s.sctx.send_terminal),
                Some(crate::error::SendTerminal::ProvisionalAbort)
            ),
            "the send slot holds the retained provisional marker"
        );

        // A connection cause is recorded on the shared slot AFTER the provisional.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
            std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            })) => assert_eq!(
                error_code, 9,
                "connection cause takes precedence over provisional"
            ),
            other => panic!("expected ApplicationClose{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn mf2_graceful_finish_does_not_commit_connection_slot() {
        // SC-002 / MF-2: a graceful finish that returns success must NOT commit the
        // connection slot even though a provisional connection cause was prepared as
        // reducer input; a subsequently-published specific cause is still delivered
        // to a later observer.
        let slot = new_conn_terminal_slot();
        let h = CountingHandle::default();
        let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
        let (sctx, mut ctx) = test_send_ctx_with_conn(slot.clone());
        let mut s = H3SendStream::with_exec(exec, sctx);
        let mut cx = noop_cx();

        // Start finish (submits graceful, sets finish_started), then queue the
        // graceful FinishComplete on the ordered channel.
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Pending
        ));
        assert_eq!(h.gracefuls.load(Relaxed), 1);
        stream_callback(
            &mut ctx,
            StreamEvent::SendShutdownComplete { graceful: true },
        )
        .unwrap();

        // A provisional connection cause is prepared as reducer input.
        record_conn_terminal(&slot, ConnectionTerminal::LocalClose);

        // The graceful finish wins and returns success WITHOUT committing the slot.
        assert!(matches!(
            SendStream::<Bytes>::poll_finish(&mut s, &mut cx),
            std::task::Poll::Ready(Ok(()))
        ));

        // The slot is still refinable (not observed): a later specific cause refines
        // it and a genuine observer delivers code 9.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        match crate::observe_terminal(&slot) {
            ConnectionErrorIncoming::ApplicationClose { error_code } => assert_eq!(
                error_code, 9,
                "graceful finish must not freeze a provisional cause"
            ),
            other => panic!("expected refined ApplicationClose{{9}}, got {other:?}"),
        }
    }

    #[test]
    fn sc003_send_delivery_locks_identity_both_orderings() {
        // SC-003 / consistency, both orderings: after a send delivery surfaces a
        // connection cause it is committed (frozen), so a later refinement does not
        // change what a subsequent accept observer (`observe_terminal`) sees.

        // (a) deliver-before-record: an immediate send error delivers and freezes.
        {
            let slot = new_conn_terminal_slot();
            let exec = Box::new(InjectingExec::new(
                slot.clone(),
                ConnectionTerminal::PeerApplication(9),
            ));
            let (sctx, _ctx) = test_send_ctx_with_conn(slot.clone());
            let mut s = H3SendStream::with_exec(exec, sctx);

            assert!(matches!(
                SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"d"))),
                Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error: ConnectionErrorIncoming::ApplicationClose { error_code: 9 },
                })
            ));
            record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
            assert!(matches!(
                crate::observe_terminal(&slot),
                ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
            ));
        }

        // (b) record-before-deliver: the cause is recorded first; a poll_ready
        //     delivery surfaces and locks it; a later refinement is still rejected.
        {
            let slot = new_conn_terminal_slot();
            let h = CountingHandle::default();
            let exec = Box::new(CountingExec::new(h, VecDeque::new()));
            let (sctx, _ctx) = test_send_ctx_with_conn(slot.clone());
            let mut s = H3SendStream::with_exec(exec, sctx);
            let mut cx = noop_cx();

            record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
            match SendStream::<Bytes>::poll_ready(&mut s, &mut cx) {
                std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
                })) => assert_eq!(error_code, 9),
                other => panic!("expected ApplicationClose{{9}}, got {other:?}"),
            }
            record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
            assert!(matches!(
                crate::observe_terminal(&slot),
                ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
            ));
        }
    }
}

/// Phase 7 downcall clamp tests: the recv-side `stop_sending` and the
/// connection-side `OpenStreams::close` route their outgoing application codes
/// through `clamp_application_code` before the native shutdown. Each is exercised
/// through its executor seam so the *actual* submitted value is asserted at the
/// varint boundary (`(1<<62)-1`, `1<<62`, `u64::MAX`) with no live stream and no
/// native shutdown reaching the wire.
#[cfg(test)]
mod downcall_clamp {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use h3::error::Code;
    use h3::quic::{ConnectionErrorIncoming, OpenStreams, RecvStream, StreamErrorIncoming};

    use crate::error::{ConnectionTerminal, clamp_application_code};
    use crate::msquic::{
        Connection, ConnectionEvent, ConnectionRef, RegistrationConfig, Status, Stream,
        StreamEvent, StreamOpenFlags, StreamRef,
    };
    use crate::registration::RundownGuard;
    use crate::{
        ConnHandle, H3RecvStream, OpenExec, OpeningStream, PreIdReceivers, PreIdTail, RecvExec,
        Registration, StreamOpener, new_conn_terminal_slot, record_conn_terminal,
        stream_ctx_channel, stream_ctx_channel_pre_id,
    };

    /// Recording recv-side seam: captures every clamped `stop_sending` code.
    #[derive(Debug, Default)]
    struct RecordingRecvExec {
        codes: Arc<Mutex<Vec<u64>>>,
    }

    impl RecvExec for RecordingRecvExec {
        fn submit_stop_sending(&self, code: u64) -> Result<(), Status> {
            self.codes.lock().unwrap().push(code);
            Ok(())
        }
    }

    /// Recording open-side seam: captures every clamped `close` code; never opens.
    #[derive(Debug, Default)]
    struct RecordingOpenExec {
        codes: Arc<Mutex<Vec<u64>>>,
    }

    impl OpenExec for RecordingOpenExec {
        fn submit_open_start(
            &self,
            _conn: &ConnHandle,
            _uni: bool,
        ) -> Result<OpeningStream, Status> {
            unimplemented!("close-clamp test never opens a stream")
        }
        fn submit_conn_shutdown(&self, _conn: &ConnHandle, code: u64) {
            self.codes.lock().unwrap().push(code);
        }
    }

    const BOUNDARY: [(u64, u64); 3] = [
        ((1u64 << 62) - 1, (1u64 << 62) - 1),
        (1u64 << 62, (1u64 << 62) - 1),
        (u64::MAX, (1u64 << 62) - 1),
    ];

    #[test]
    fn stop_sending_submits_clamped_code_via_seam() {
        for (input, expected) in BOUNDARY {
            let rec = RecordingRecvExec::default();
            let codes = rec.codes.clone();
            let id: h3::quic::StreamId = 0u64.try_into().unwrap();
            let (_ctx, _sctx, rctx) = stream_ctx_channel(id);
            let mut r = H3RecvStream::with_exec(Box::new(rec), rctx);

            RecvStream::stop_sending(&mut r, input);
            assert_eq!(
                codes.lock().unwrap().as_slice(),
                &[expected],
                "stop_sending must submit the clamped code (input {input:#x})"
            );
            // Cross-check the clamp helper agrees with the submitted value.
            assert_eq!(clamp_application_code(input), expected);
        }
    }

    #[test]
    fn open_streams_close_submits_clamped_code_via_seam() {
        // A single real (unstarted) connection is reused; the recording seam
        // records the clamped code without any native shutdown reaching the wire.
        let reg = Registration::new(&RegistrationConfig::default()).unwrap();

        for (input, expected) in BOUNDARY {
            let inner =
                Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| Ok(())).unwrap();
            let guard = RundownGuard::new(reg.state().clone());
            let conn = Arc::new(ConnHandle::new(inner, guard, new_conn_terminal_slot()));

            let rec = RecordingOpenExec::default();
            let codes = rec.codes.clone();
            let mut opener = StreamOpener::with_open_exec(conn, Box::new(rec));

            OpenStreams::<Bytes>::close(&mut opener, Code::from(input), b"");
            assert_eq!(
                codes.lock().unwrap().as_slice(),
                &[expected],
                "close must submit the clamped code (input {input:#x})"
            );
            assert_eq!(clamp_application_code(input), expected);
            // Drop the opener (and its ConnHandle) before the next iteration so the
            // native ConnectionClose runs while the registration is still alive.
            drop(opener);
        }
    }

    /// A leaked no-op waker gives a `'static` context reusable across polls.
    fn noop_cx() -> std::task::Context<'static> {
        let waker = Box::leak(Box::new(futures::task::noop_waker()));
        std::task::Context::from_waker(waker)
    }

    /// Open-side seam that models the `ShutdownComplete`-drops-the-start-sender
    /// race (MF / SF): `submit_open_start` opens a real (unstarted) native stream
    /// but immediately DROPS the pre-ID start sender, so the [`OpeningStream`]'s
    /// `start` receiver resolves as `oneshot::Canceled` — exactly the condition
    /// `poll_open_inner`'s cancellation branch handles. When `record` is set, it
    /// also publishes that connection terminal into the shared slot *during*
    /// `submit_open_start` — i.e. AFTER `poll_open_inner`'s step-1 fail-fast has
    /// already seen an empty slot and BEFORE it polls the pending start — modelling
    /// a connection close that raced the open, so the cancellation resolves through
    /// [`crate::stream_open_conn_error`] to a connection error rather than the
    /// fail-fast path.
    #[derive(Debug)]
    struct CancellingOpenExec {
        record: Option<ConnectionTerminal>,
    }

    impl OpenExec for CancellingOpenExec {
        fn submit_open_start(&self, conn: &ConnHandle, uni: bool) -> Result<OpeningStream, Status> {
            // Publish the racing terminal (if any) now: after step-1 fail-fast, so
            // it is only observed at the cancellation branch's terminal read.
            if let Some(reason) = self.record.clone() {
                record_conn_terminal(conn.terminal(), reason);
            }
            let (ctx, recv) = stream_ctx_channel_pre_id(conn.terminal().clone());
            let flag = if uni {
                StreamOpenFlags::UNIDIRECTIONAL
            } else {
                StreamOpenFlags::NONE
            };
            // Trivial handler (never fires on an unstarted stream); it does NOT
            // capture `ctx`, so dropping `ctx` drops the start sender.
            let s = Stream::open(conn, flag, |_: StreamRef, _: StreamEvent| Ok(()))?;
            drop(ctx); // drop the start sender -> pending start resolves Canceled
            let PreIdReceivers {
                start,
                send,
                send_terminal,
                conn_terminal,
                receive,
                recv_budget,
            } = recv;
            Ok(OpeningStream {
                stream: Arc::new(s),
                start,
                tail: PreIdTail {
                    send,
                    send_terminal,
                    conn_terminal,
                    receive,
                    recv_budget,
                },
            })
        }
        fn submit_conn_shutdown(&self, _conn: &ConnHandle, _code: u64) {
            unimplemented!("cancellation drive never closes the connection")
        }
    }

    /// Item 2 (Phase 8): drive the REAL `poll_open_inner` (via the public
    /// `poll_open_bidi` / `poll_open_send`) through its `ShutdownComplete`
    /// cancellation branch and assert the mapped `StreamErrorIncoming` comes OUT of
    /// the real function — not merely a detached `Canceled` detection plus a helper
    /// call. Both sub-outcomes of the branch are covered: cancelled-with-no-reason
    /// (nested `InternalError`) and cancelled-by-a-connection-close (a real
    /// connection error carrying the peer code).
    #[test]
    fn poll_open_inner_start_cancellation_maps_through_real_function() {
        let reg = Registration::new(&RegistrationConfig::default()).unwrap();

        // (a) Cancelled with NO published reason -> nested InternalError, straight
        //     out of the real poll_open_bidi.
        {
            let inner =
                Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| Ok(())).unwrap();
            let guard = RundownGuard::new(reg.state().clone());
            let conn = Arc::new(ConnHandle::new(inner, guard, new_conn_terminal_slot()));
            let mut opener =
                StreamOpener::with_open_exec(conn, Box::new(CancellingOpenExec { record: None }));
            let mut cx = noop_cx();

            match OpenStreams::<Bytes>::poll_open_bidi(&mut opener, &mut cx) {
                std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error: ConnectionErrorIncoming::InternalError(msg),
                })) => assert!(
                    msg.contains("cancelled without a terminal reason"),
                    "unexpected InternalError message: {msg}"
                ),
                other => panic!("expected InternalError from real poll_open_bidi, got {other:?}"),
            }
            drop(opener);
        }

        // (b) Cancelled by a CONNECTION close -> the real cancellation branch reads
        //     the raced terminal via stream_open_conn_error and surfaces the peer
        //     application code, straight out of the real poll_open_send.
        {
            let inner =
                Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| Ok(())).unwrap();
            let guard = RundownGuard::new(reg.state().clone());
            let conn = Arc::new(ConnHandle::new(inner, guard, new_conn_terminal_slot()));
            let mut opener = StreamOpener::with_open_exec(
                conn,
                Box::new(CancellingOpenExec {
                    record: Some(ConnectionTerminal::PeerApplication(3)),
                }),
            );
            let mut cx = noop_cx();

            match OpenStreams::<Bytes>::poll_open_send(&mut opener, &mut cx) {
                std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
                })) => assert_eq!(error_code, 3, "peer application code preserved"),
                other => {
                    panic!("expected ApplicationClose{{3}} from real poll_open_send, got {other:?}")
                }
            }
            drop(opener);
        }
    }

    #[test]
    fn poll_open_inner_fail_fast_commits_connection_cause_both_orderings() {
        // Phase 1 / SF-C (SC-003): the REAL `poll_open_inner` fail-fast delivery
        // (step 1) commits (freezes) the connection slot on delivery, so a later
        // refinement does not change what a subsequent observer sees — verified for
        // both callback orderings against a real (unstarted) connection.
        let reg = Registration::new(&RegistrationConfig::default()).unwrap();

        // (a) record-before-deliver: the cause is recorded, then the open delivers
        //     and freezes it; a later refinement is rejected.
        {
            let inner =
                Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| Ok(())).unwrap();
            let guard = RundownGuard::new(reg.state().clone());
            let conn = Arc::new(ConnHandle::new(inner, guard, new_conn_terminal_slot()));
            record_conn_terminal(conn.terminal(), ConnectionTerminal::PeerApplication(9));
            let mut opener = StreamOpener::with_open_exec(
                conn.clone(),
                Box::new(CancellingOpenExec { record: None }),
            );
            let mut cx = noop_cx();

            match OpenStreams::<Bytes>::poll_open_bidi(&mut opener, &mut cx) {
                std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
                })) => assert_eq!(error_code, 9, "fail-fast delivers the recorded cause"),
                other => panic!("expected ApplicationClose{{9}} from fail-fast, got {other:?}"),
            }
            // The fail-fast delivery froze the slot: a later refinement is rejected.
            record_conn_terminal(conn.terminal(), ConnectionTerminal::PeerApplication(7));
            assert!(matches!(
                crate::observe_terminal(conn.terminal()),
                ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
            ));
            drop(opener);
        }

        // (b) empty-then-record on a fresh open: with no cause the fail-fast does
        //     not fire; a cause recorded before the next poll is then delivered and
        //     frozen, and a later refinement is rejected.
        {
            let inner =
                Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| Ok(())).unwrap();
            let guard = RundownGuard::new(reg.state().clone());
            let conn = Arc::new(ConnHandle::new(inner, guard, new_conn_terminal_slot()));
            record_conn_terminal(conn.terminal(), ConnectionTerminal::PeerApplication(5));
            let mut opener = StreamOpener::with_open_exec(
                conn.clone(),
                Box::new(CancellingOpenExec { record: None }),
            );
            let mut cx = noop_cx();

            match OpenStreams::<Bytes>::poll_open_send(&mut opener, &mut cx) {
                std::task::Poll::Ready(Err(StreamErrorIncoming::ConnectionErrorIncoming {
                    connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
                })) => assert_eq!(error_code, 5),
                other => panic!("expected ApplicationClose{{5}} from fail-fast, got {other:?}"),
            }
            record_conn_terminal(conn.terminal(), ConnectionTerminal::PeerApplication(6));
            assert!(matches!(
                crate::observe_terminal(conn.terminal()),
                ConnectionErrorIncoming::ApplicationClose { error_code: 5 }
            ));
            drop(opener);
        }
    }

    /// Build a real (unstarted) adapter [`crate::Connection`] whose incoming-stream
    /// channels and shared terminal slot the test controls, so the actual
    /// `poll_accept_recv`/`poll_accept_bidi` frontends can be driven without a
    /// live peer. The returned [`crate::ConnCtxSender`] owns the `uni`/`bidi`
    /// senders: dropping it closes both channels (`poll_next` → `None`), which is
    /// exactly the channel-closure path the accept frontends terminate on.
    ///
    /// Drop order matters: the caller binds `(reg, connection, ctx)` so that at
    /// end of scope `connection` (and its `ConnHandle`) drops before `reg`, i.e.
    /// the native `ConnectionClose` runs while the registration is still alive.
    fn accept_frontend_connection() -> (Registration, crate::Connection, crate::ConnCtxSender) {
        let reg = Registration::new(&RegistrationConfig::default()).unwrap();
        let inner =
            Connection::open(reg.raw(), |_: ConnectionRef, _: ConnectionEvent| Ok(())).unwrap();
        let guard = RundownGuard::new(reg.state().clone());
        let (ctx, crx) = crate::conn_ctx_channel();
        let conn = Arc::new(ConnHandle::new(inner, guard, crx.terminal.clone()));
        let opener = StreamOpener::new(conn.clone());
        let connection = crate::Connection {
            conn,
            ctx: crx,
            opener,
        };
        (reg, connection, ctx)
    }

    #[test]
    fn poll_accept_recv_delivering_cause_commits_and_locks_identity() {
        // Phase 1 / Fix 1 (SC-003) — accept as a COMMITTING poll, cause-delivery
        // ordering, driven through the real `poll_accept_recv` frontend: a
        // connection cause is recorded, the incoming-stream channel then closes,
        // and the accept poll delivers+commits (freezes) the cause. A later
        // refinement is rejected because delivery froze the slot.
        let (_reg, mut connection, ctx) = accept_frontend_connection();
        let slot = connection.ctx.terminal.clone();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        drop(ctx); // close the uni/bidi channels: accept observes channel closure
        let mut cx = noop_cx();
        match <crate::Connection as h3::quic::Connection<Bytes>>::poll_accept_recv(
            &mut connection,
            &mut cx,
        ) {
            std::task::Poll::Ready(Err(ConnectionErrorIncoming::ApplicationClose {
                error_code,
            })) => assert_eq!(error_code, 9),
            other => panic!("expected ApplicationClose{{9}} from accept delivery, got {other:?}"),
        }
        // Delivery froze the slot: a later different cause does not change what a
        // subsequent observer sees.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
        drop(connection);
    }

    #[test]
    fn poll_accept_recv_empty_slot_non_delivery_does_not_commit() {
        // Phase 1 / Fix 1 core — accept as a NON-delivery, empty-slot ordering,
        // driven through the real `poll_accept_recv` frontend: the channel closes
        // with NO recorded terminal, so the poll returns the synthetic
        // `InternalError` WITHOUT freezing. A subsequently-recorded genuine cause
        // is therefore still observable by a later consumer.
        let (_reg, mut connection, ctx) = accept_frontend_connection();
        let slot = connection.ctx.terminal.clone();
        drop(ctx); // close the channels with an empty slot
        let mut cx = noop_cx();
        match <crate::Connection as h3::quic::Connection<Bytes>>::poll_accept_recv(
            &mut connection,
            &mut cx,
        ) {
            std::task::Poll::Ready(Err(ConnectionErrorIncoming::InternalError(msg))) => {
                assert!(msg.contains("without a terminal reason"), "msg: {msg}");
            }
            other => panic!("expected InternalError from empty-slot non-delivery, got {other:?}"),
        }
        // The non-delivery did not freeze: a later genuine cause is delivered.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
        drop(connection);
    }

    #[test]
    fn poll_accept_bidi_delivering_cause_commits_and_locks_identity() {
        // Same committing-poll delivery invariant as the recv case, driven through
        // the real `poll_accept_bidi` frontend (both accept frontends share the
        // `observe_terminal` → `commit_conn` delivery seam).
        let (_reg, mut connection, ctx) = accept_frontend_connection();
        let slot = connection.ctx.terminal.clone();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        drop(ctx);
        let mut cx = noop_cx();
        match <crate::Connection as h3::quic::Connection<Bytes>>::poll_accept_bidi(
            &mut connection,
            &mut cx,
        ) {
            std::task::Poll::Ready(Err(ConnectionErrorIncoming::ApplicationClose {
                error_code,
            })) => assert_eq!(error_code, 9),
            other => panic!("expected ApplicationClose{{9}} from accept delivery, got {other:?}"),
        }
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
        drop(connection);
    }

    #[test]
    fn poll_accept_bidi_empty_slot_non_delivery_does_not_commit() {
        // Empty-slot non-delivery invariant driven through the real
        // `poll_accept_bidi` frontend.
        let (_reg, mut connection, ctx) = accept_frontend_connection();
        let slot = connection.ctx.terminal.clone();
        drop(ctx);
        let mut cx = noop_cx();
        match <crate::Connection as h3::quic::Connection<Bytes>>::poll_accept_bidi(
            &mut connection,
            &mut cx,
        ) {
            std::task::Poll::Ready(Err(ConnectionErrorIncoming::InternalError(msg))) => {
                assert!(msg.contains("without a terminal reason"), "msg: {msg}");
            }
            other => panic!("expected InternalError from empty-slot non-delivery, got {other:?}"),
        }
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            crate::observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
        drop(connection);
    }
}
/// msquic connection, mirroring `listener::basic_server_test` but driving the
/// raw `h3::quic` trait surface (open/accept bidi streams, `send_data`,
/// `reset`, `stop_sending`, connection `close`) directly so each error-
/// propagation path can be triggered deterministically without the full h3
/// protocol layer.
///
/// Determinism strategy (no arbitrary sleeps racing real timers):
/// - each test owns an isolated [`Registration`] and an **ephemeral** loopback
///   port (`127.0.0.1:0`, queried back via `get_local_addr`), so parallel tests
///   never collide;
/// - peer `RESET_STREAM` / `STOP_SENDING` / application-close are produced by
///   *calling the peer endpoint's own* `reset` / `stop_sending` / `close`, not by
///   hoping a timer fires;
/// - the idle-timeout case sets a short `IdleTimeoutMs` via msquic settings and
///   then simply awaits the (bounded) shutdown — a fixed setting, not a race
///   against another timer;
/// - the accepted-stream-ID failpoint is armed on the **live** server
///   `Connection` (Phase 5 seam) *before* the peer opens its stream, with an
///   explicit client→server handshake barrier so the arming strictly precedes
///   the peer `PeerStreamStarted`.
///
/// SOURCE-REVIEW-ONLY GUARANTEE (SC-007, labelled UNTESTED): the close-time
/// inline `SendComplete` drain performed by native `QuicStreamClose` has **no**
/// executable drop-triggered teardown test here — the public API cannot hold a
/// real send observably outstanding across the close (buffered sends complete
/// synchronously; see `docs/testing.md`, "Native-test mechanisms").
/// That guarantee rests on native source review plus the binding's uniform
/// `close_inner` contract, and is asserted only by
/// [`conformance::close_time_inline_drain_is_source_review_only`] as a labelled,
/// auditable NON-executable marker — never by a passing teardown test. The
/// adapter's own exactly-once reclamation bookkeeping is proven by the
/// `send_seam` `CountingExec` suite, which replays the real `stream_callback`
/// reclaim path.
#[cfg(test)]
mod conformance {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use bytes::Bytes;
    use h3::error::Code;
    use h3::proto::frame::Frame;
    use h3::quic::{
        ConnectionErrorIncoming, OpenStreams, RecvStream, SendStream, StreamErrorIncoming,
    };
    use msquic::{
        BufferRef, CredentialConfig, CredentialFlags, RegistrationConfig, ServerResumptionLevel,
        Settings,
    };

    use crate::test::util::{get_test_cred, try_setup_tracing};
    use crate::{Connection, H3_INTERNAL_ERROR, H3Stream, Listener, Registration};

    // ── poll_fn adapters over the &mut self trait methods (buffer type = Bytes) ──

    async fn open_bidi(conn: &mut Connection) -> Result<H3Stream, StreamErrorIncoming> {
        std::future::poll_fn(|cx| OpenStreams::<Bytes>::poll_open_bidi(conn, cx)).await
    }

    async fn accept_bidi(conn: &mut Connection) -> Result<H3Stream, ConnectionErrorIncoming> {
        std::future::poll_fn(|cx| {
            <Connection as h3::quic::Connection<Bytes>>::poll_accept_bidi(conn, cx)
        })
        .await
    }

    async fn send_ready<S: SendStream<Bytes>>(s: &mut S) -> Result<(), StreamErrorIncoming> {
        std::future::poll_fn(|cx| s.poll_ready(cx)).await
    }

    async fn send_finish<S: SendStream<Bytes>>(s: &mut S) -> Result<(), StreamErrorIncoming> {
        std::future::poll_fn(|cx| s.poll_finish(cx)).await
    }

    /// Await a peer-caused send-side termination (`STOP_SENDING`).
    ///
    /// An idle `poll_ready` returns `Ok` immediately (no send in flight, no
    /// terminal yet), so a single poll can observe `Ok` before the peer's
    /// `STOP_SENDING` frame has been delivered and turned into the sticky send
    /// terminal. This re-polls on a short fixed interval until the terminal is
    /// observed. It is NOT a race against a timer: the peer stop is guaranteed to
    /// arrive over loopback, so the loop always exits via the terminal; the outer
    /// bound only guards against a hang if the propagation invariant regressed.
    async fn await_peer_send_terminated<S: SendStream<Bytes>>(s: &mut S) -> u64 {
        let poll_terminal = async {
            loop {
                match send_ready(s).await {
                    Err(StreamErrorIncoming::StreamTerminated { error_code }) => {
                        return error_code;
                    }
                    Err(other) => panic!("expected StreamTerminated, got {other:?}"),
                    // Terminal not yet delivered; yield and re-poll.
                    Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), poll_terminal)
            .await
            .expect("peer STOP_SENDING must be observed on the send side")
    }

    async fn recv_next<R: RecvStream>(
        r: &mut R,
    ) -> Result<Option<<R as RecvStream>::Buf>, StreamErrorIncoming> {
        std::future::poll_fn(|cx| r.poll_data(cx)).await
    }

    /// A live loopback pair plus the machinery that must outlive them.
    fn run_loopback<F, Fut>(idle_ms: u64, body: F)
    where
        F: FnOnce(Connection, Connection) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        try_setup_tracing();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let reg = Registration::new(&RegistrationConfig::default()).unwrap();
            let alpn = [BufferRef::from("h3")];

            // Server: self-signed cert, generous peer stream credit, configurable
            // idle timeout.
            let cred = get_test_cred();
            let server_settings = Settings::new()
                .set_ServerResumptionLevel(ServerResumptionLevel::ResumeAndZerortt)
                .set_PeerBidiStreamCount(100)
                .set_PeerUnidiStreamCount(100)
                .set_IdleTimeoutMs(idle_ms);
            let server_config = reg
                .open_configuration(&alpn, Some(&server_settings))
                .unwrap();
            let cred_config = CredentialConfig::new()
                .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION)
                .set_credential(cred);
            server_config.load_credential(&cred_config).unwrap();
            let server_config = Arc::new(server_config);

            let mut listener = Listener::new(
                &reg,
                server_config.clone(),
                &alpn,
                Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
            )
            .unwrap();
            // Ephemeral port assigned by ListenerStart; read it back so the client
            // dials the exact bound port (no fixed-port collisions across tests).
            let port = listener.get_ref().get_local_addr().unwrap().port();

            let client_settings = Settings::new()
                .set_PeerBidiStreamCount(100)
                .set_PeerUnidiStreamCount(100)
                .set_IdleTimeoutMs(idle_ms);
            let client_config = reg
                .open_configuration(&alpn, Some(&client_settings))
                .unwrap();
            let client_cred = CredentialConfig::new_client()
                .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION);
            client_config.load_credential(&client_cred).unwrap();

            // Drive server accept and client connect concurrently on the same
            // single-threaded runtime; msquic worker threads fire the callbacks.
            let (accepted, connected) = tokio::join!(
                listener.accept(),
                Connection::connect(&reg, &client_config, "127.0.0.1", port),
            );
            let server = accepted.expect("server accept ok").expect("a connection");
            let client = connected.expect("client connect ok");

            // Run the scenario; `body` owns and drops both connections.
            body(server, client).await;

            // Deterministic teardown order: connections were dropped by `body`;
            // drop the listener, then drain the rundown, then the configurations.
            drop(listener);
            reg.shutdown();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), reg.wait_idle()).await;
            drop(server_config);
            drop(client_config);
        });
    }

    /// Send one h3 `DATA` frame carrying `payload` on a send half.
    fn send_data_frame<S: SendStream<Bytes>>(
        s: &mut S,
        payload: &'static [u8],
    ) -> Result<(), StreamErrorIncoming> {
        s.send_data(Frame::Data(Bytes::from_static(payload)))
    }

    /// Client opens a bidi stream and sends `first` so the server's
    /// `PeerStreamStarted` fires; returns `(client_stream, server_stream)`, both
    /// fully identified. The client flush (`poll_ready` to completion) guarantees
    /// the opening `STREAM` frame has reached the peer before the server accepts,
    /// so the handoff is ordered, not raced.
    async fn establish_bidi(
        client: &mut Connection,
        server: &mut Connection,
        first: &'static [u8],
    ) -> (H3Stream, H3Stream) {
        let mut cs = open_bidi(client).await.expect("client open bidi");
        send_data_frame(&mut cs, first).expect("client send first frame");
        send_ready(&mut cs).await.expect("client flush first frame");
        let ss = accept_bidi(server).await.expect("server accept bidi");
        (cs, ss)
    }

    /// (a) Peer `RESET_STREAM` → `StreamTerminated { code }` on the receiving
    /// side, carrying the exact HTTP/3 code. The server resets its send half of a
    /// client-opened bidi stream; the client observes the reset at `poll_data`.
    #[test]
    fn peer_reset_stream_maps_to_stream_terminated() {
        run_loopback(5_000, |mut server, mut client| async move {
            const RESET_CODE: u64 = 0x4142;
            let (mut cs, mut ss) = establish_bidi(&mut client, &mut server, b"ping").await;

            // Server RESET_STREAMs its send direction with a specific code.
            SendStream::<Bytes>::reset(&mut ss, RESET_CODE);

            // Client's receive half observes the peer reset (after draining any
            // bytes the server may have sent first — none are expected here).
            loop {
                match recv_next(&mut cs).await {
                    Ok(Some(_)) => continue,
                    Ok(None) => panic!("expected RESET_STREAM, got a clean FIN"),
                    Err(StreamErrorIncoming::StreamTerminated { error_code }) => {
                        assert_eq!(error_code, RESET_CODE, "exact peer reset code preserved");
                        break;
                    }
                    Err(other) => panic!("expected StreamTerminated, got {other:?}"),
                }
            }
            drop(cs);
            drop(ss);
            drop(server);
            drop(client);
        });
    }

    /// (b, idle) Peer `STOP_SENDING` observed from the *send* side with no send
    /// outstanding: the server stop_sends the receive half of a client-opened
    /// bidi stream; the client's idle `poll_ready` surfaces
    /// `StreamTerminated { code }`.
    #[test]
    fn peer_stop_sending_observed_from_send_side_idle() {
        run_loopback(5_000, |mut server, mut client| async move {
            const STOP_CODE: u64 = 0x5253;
            let (mut cs, mut ss) = establish_bidi(&mut client, &mut server, b"hi").await;

            // Server STOP_SENDINGs its receive half (= client's send half).
            RecvStream::stop_sending(&mut ss, STOP_CODE);

            // Client's send half — idle, no data buffered — observes the stop.
            let code = await_peer_send_terminated(&mut cs).await;
            assert_eq!(code, STOP_CODE, "exact peer stop code preserved (idle)");
            drop(cs);
            drop(ss);
            drop(server);
            drop(client);
        });
    }

    // (b, in flight) DEFERRED-TO-SEAM. A peer `STOP_SENDING` observed with a send
    // *genuinely outstanding* is intentionally NOT covered by a loopback test:
    // over pure 127.0.0.1 msquic copies a buffered `send_data` and completes it
    // synchronously (often before `send_data` even returns), so a send cannot be
    // *held* observably outstanding through the public API — a loopback test could
    // claim "in flight" but never prove it (documented in "Native-test
    // mechanisms", `docs/testing.md`). The true outstanding-send
    // condition is instead proven deterministically at the send seam by
    // [`send_seam::peer_stop_sending_observed_with_send_outstanding`], where the
    // `CountingExec` retains the native-owned buffer so the send is provably still
    // outstanding at the exact moment STOP_SENDING is observed and surfaced at
    // `poll_ready` as `StreamTerminated { code }`. The idle observation over real
    // loopback remains covered by
    // [`peer_stop_sending_observed_from_send_side_idle`] above.

    /// (d) Idle timeout → `ConnectionErrorIncoming::Timeout`. A short, fixed
    /// `IdleTimeoutMs` makes the transport idle-close deterministic (a setting,
    /// not a race against another timer); the client's `poll_accept_bidi` awaits
    /// the resulting terminal with no manual sleep.
    #[test]
    fn idle_timeout_maps_to_timeout() {
        run_loopback(300, |server, mut client| async move {
            // No traffic after connect: the negotiated 300 ms idle timeout fires
            // and both endpoints transport-close as idle.
            let err = accept_bidi(&mut client)
                .await
                .expect_err("client must observe the idle timeout");
            assert!(
                matches!(err, ConnectionErrorIncoming::Timeout),
                "expected Timeout, got {err:?}"
            );
            drop(server);
            drop(client);
        });
    }

    /// (e, local reset) Local cancellation via `reset(code)` → the client's own
    /// send half reports the local-reset outcome (`Unknown(LocalStreamReset)`),
    /// issues at most one native `ABORT_SEND`, and never panics.
    #[test]
    fn local_reset_yields_local_stream_reset_outcome() {
        run_loopback(5_000, |mut server, mut client| async move {
            const LOCAL_CODE: u64 = 0x0707;
            let (mut cs, ss) = establish_bidi(&mut client, &mut server, b"payload").await;

            // Client locally resets its own send half (infallible).
            SendStream::<Bytes>::reset(&mut cs, LOCAL_CODE);

            // The local outcome is surfaced at poll_finish as a local reset, not a
            // peer termination or connection error.
            match send_finish(&mut cs).await {
                Err(StreamErrorIncoming::Unknown(e)) => {
                    assert!(
                        e.downcast_ref::<crate::LocalStreamReset>().is_some(),
                        "expected LocalStreamReset, got {e:?}"
                    );
                }
                other => panic!("expected Unknown(LocalStreamReset), got {other:?}"),
            }
            drop(cs);
            drop(ss);
            drop(server);
            drop(client);
        });
    }

    /// (e, local stop_sending) Local cancellation via `stop_sending(code)` → the
    /// client's own receive half ends cleanly (`Ok(None)`, SF-6 local EOF)
    /// without panicking, regardless of the peer.
    #[test]
    fn local_stop_sending_yields_clean_local_eof() {
        run_loopback(5_000, |mut server, mut client| async move {
            const LOCAL_CODE: u64 = 0x0809;
            let (mut cs, ss) = establish_bidi(&mut client, &mut server, b"payload").await;

            // Client locally stop_sends its own receive half (infallible).
            RecvStream::stop_sending(&mut cs, LOCAL_CODE);

            // SF-6: our own stop_sending is a clean local end-of-stream.
            match recv_next(&mut cs).await {
                Ok(None) => {}
                other => panic!("expected clean Ok(None) local EOF, got {other:?}"),
            }
            drop(cs);
            drop(ss);
            drop(server);
            drop(client);
        });
    }

    /// (f) Attachment failure — a stream open attempted after the connection has
    /// closed → a connection error, no panic. The peer application-closes the
    /// connection; once the client has observed that terminal, a fresh
    /// `poll_open_bidi` fails fast with a `ConnectionErrorIncoming` rather than
    /// panicking.
    ///
    /// The tighter *in-flight* variant — a pending `OpeningStream` whose
    /// `StartComplete` receiver is cancelled by `ShutdownComplete` — cannot be
    /// suspended at exactly that instant through the public loopback surface, so
    /// it is proven deterministically by the hermetic seam test
    /// `downcall_clamp::poll_open_inner_start_cancellation_maps_through_real_function`.
    #[test]
    fn open_after_connection_close_is_connection_error_no_panic() {
        run_loopback(5_000, |mut server, mut client| async move {
            const APP_CODE: u64 = 0x0abc;
            OpenStreams::<Bytes>::close(&mut server, Code::from(APP_CODE), b"bye");

            // Confirm the client has observed the connection terminal first.
            let conn_err = accept_bidi(&mut client)
                .await
                .expect_err("client observes the peer close");
            assert!(
                matches!(conn_err, ConnectionErrorIncoming::ApplicationClose { error_code } if error_code == APP_CODE),
                "expected ApplicationClose({APP_CODE:#x}), got {conn_err:?}"
            );

            // Now an open must fail fast with a connection error — never a panic.
            match open_bidi(&mut client).await {
                Err(StreamErrorIncoming::ConnectionErrorIncoming { .. }) => {}
                other => panic!("expected ConnectionErrorIncoming, got {other:?}"),
            }
            drop(server);
            drop(client);
        });
    }

    /// (g) Accepted-send reclamation *ordering* over a real loopback connection:
    /// a server-accepted send drives to completion (proving the native
    /// `SendComplete` was delivered and consumed), the client receives the data
    /// end to end, and dropping every frontend owner afterwards causes no
    /// callback panic.
    ///
    /// NOTE ON RECLAIM-ONCE: the exactly-once `Box<SendBuffer>` reclamation
    /// *count* is proven by the `send_seam` `CountingExec` tests
    /// (`send_buffer_reclaimed_exactly_once_via_callback`,
    /// `immediate_send_failure_reclaims_without_completion`, and the outstanding-
    /// retain matrix), which replay the production `stream_callback` reclaim path
    /// with a drop-counted buffer. The public `send_data` path builds its
    /// `SendBuffer` internally with no injectable counter, so a real loopback send
    /// cannot be *counted* here — only its ordering and no-panic teardown are
    /// observable (see `docs/testing.md`, "Native-test mechanisms").
    #[test]
    fn accepted_send_completes_and_teardown_is_panic_free() {
        run_loopback(5_000, |mut server, mut client| async move {
            let (mut cs, mut ss) = establish_bidi(&mut client, &mut server, b"req").await;

            // The accepted (server) side sends a response and drives it to
            // completion: poll_ready resolves ready only after the single native
            // SendComplete for this send is delivered and consumed.
            const RESP: &[u8] = b"accepted-response-body";
            send_data_frame(&mut ss, RESP).expect("server send response");
            send_ready(&mut ss)
                .await
                .expect("server send completes once");

            // The client receives the response bytes end to end (the h3 DATA frame
            // header precedes the payload, which appears as the frame's suffix).
            let mut got = Vec::new();
            loop {
                match recv_next(&mut cs).await {
                    Ok(Some(chunk)) => {
                        use bytes::Buf as _;
                        got.extend_from_slice(chunk.chunk());
                        if got.ends_with(RESP) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => panic!("client recv error: {e:?}"),
                }
            }
            assert!(
                got.ends_with(RESP),
                "client must receive the accepted-send payload; got {got:?}"
            );

            // Drop every frontend owner after the completion: no callback panic.
            drop(cs);
            drop(ss);
            drop(server);
            drop(client);
        });
    }

    /// (h) Accepted-stream ID failpoint (Phase 5 connection-scoped seam), armed
    /// on the *live* server `Connection` BEFORE the peer opens its stream. The
    /// rejected stream is closed natively (never enqueued), the server's accept
    /// path fails fast with `InternalError`, and — mirroring what h3 does on that
    /// internal error — the connection is closed with `H3_INTERNAL_ERROR`, which
    /// the peer observes as an application close carrying that code. No panic.
    #[test]
    fn accepted_stream_id_failpoint_rejects_and_closes_h3_internal_error() {
        run_loopback(5_000, |mut server, mut client| async move {
            // Arm BEFORE the peer opens a stream. Arming is synchronous on the
            // live Connection's shared atomic, and the client only opens its
            // stream afterwards, so the PeerStreamStarted callback is guaranteed
            // to see the armed failpoint.
            server.arm_accepted_id_query_fail();

            // Peer opens a bidi stream and sends, driving the server's
            // PeerStreamStarted (which trips the failpoint and rejects natively).
            let mut cs = open_bidi(&mut client).await.expect("client open bidi");
            send_data_frame(&mut cs, b"trigger").expect("client send trigger");

            // The server accept path fails fast with an internal error, and the
            // rejected stream is never delivered.
            let acc_err = accept_bidi(&mut server)
                .await
                .expect_err("rejected accept must be an internal error");
            assert!(
                matches!(acc_err, ConnectionErrorIncoming::InternalError(_)),
                "expected InternalError, got {acc_err:?}"
            );

            // h3 responds to an InternalError from the trait by closing the
            // connection with H3_INTERNAL_ERROR; emulate that here.
            OpenStreams::<Bytes>::close(&mut server, Code::H3_INTERNAL_ERROR, b"internal");

            // The peer observes the H3_INTERNAL_ERROR application close.
            let peer_err = accept_bidi(&mut client)
                .await
                .expect_err("client observes the H3_INTERNAL_ERROR close");
            match peer_err {
                ConnectionErrorIncoming::ApplicationClose { error_code } => {
                    assert_eq!(
                        error_code, H3_INTERNAL_ERROR,
                        "connection closed with H3_INTERNAL_ERROR"
                    );
                }
                other => panic!("expected ApplicationClose(H3_INTERNAL_ERROR), got {other:?}"),
            }
            drop(cs);
            drop(server);
            drop(client);
        });
    }

    /// SC-007 labelled marker (NON-executable): the close-time inline
    /// `SendComplete` drain performed by native `QuicStreamClose` has **no**
    /// drop-triggered teardown test — the public API cannot hold a real send
    /// observably outstanding across the close, so that path is exercised by
    /// **native source review + the uniform `close_inner` contract**, NOT by any
    /// executable test here. This test exists solely as an auditable label; it
    /// deliberately asserts nothing about runtime behavior (there is nothing
    /// test-observable to assert), only that this guarantee is documented as
    /// source-review-only. See "Native stream teardown on drop" in
    /// `docs/callback-safety.md` and "Native-test mechanisms" in `docs/testing.md`.
    #[test]
    fn close_time_inline_drain_is_source_review_only() {
        // Intentionally empty: the inline-drain guarantee is established by
        // source review, not by a drop-triggered teardown assertion. The adapter
        // exactly-once reclamation bookkeeping it relies on is covered by the
        // `send_seam` CountingExec tests.
    }

    /// (c) Peer application close → `ApplicationClose { code }` with the exact
    /// HTTP/3 code, observed on the other endpoint's connection accept path.
    #[test]
    fn peer_application_close_propagates_code() {
        run_loopback(5_000, |mut server, mut client| async move {
            const APP_CODE: u64 = 0x1234;
            // Server closes the connection with a specific application code.
            OpenStreams::<Bytes>::close(&mut server, Code::from(APP_CODE), b"bye");

            // Client observes the peer application close carrying that code.
            let err = accept_bidi(&mut client)
                .await
                .expect_err("client must observe the peer close");
            match err {
                ConnectionErrorIncoming::ApplicationClose { error_code } => {
                    assert_eq!(error_code, APP_CODE, "exact HTTP/3 code preserved");
                }
                other => panic!("expected ApplicationClose({APP_CODE:#x}), got {other:?}"),
            }
            drop(server);
            drop(client);
        });
    }
}

/// SC-008 NEGATIVE configuration check (SF-L). This is an *expected-FAILURE*
/// assertion, NOT an `--all-features`/both-enabled success gate: it spawns a
/// nested `cargo check` that enables BOTH mutually-exclusive provenance features
/// and asserts the build FAILS with the upstream `msquic` build-script message
/// `feature src and find are mutually exclusive`. The failure originates in the
/// upstream dependency's build script (which runs before this crate compiles), so
/// the crate does not (and cannot) intercept the both-enabled case at crate level
/// — this test asserts the failure's presence and origin, nothing more.
///
/// `#[ignore]`d because it drives a real `cargo` subprocess (slow, and it uses a
/// separate `CARGO_TARGET_DIR` to avoid the outer build lock). Run it explicitly:
/// `cargo test --no-default-features --features native-find -- --ignored both_features_mutually_exclusive_negative`
#[cfg(test)]
mod feature_config_negative {
    #[test]
    #[ignore = "NEGATIVE config check: spawns a nested `cargo check` expected to FAIL; run manually with --ignored"]
    fn both_features_mutually_exclusive_negative() {
        use std::process::Command;

        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        // Isolated target dir so this nested invocation does not contend for the
        // outer `cargo test` build lock.
        let neg_target = format!("{manifest_dir}/target/neg-both-features");

        let output = Command::new(&cargo)
            .current_dir(manifest_dir)
            .env("CARGO_TARGET_DIR", &neg_target)
            .args([
                "check",
                "--no-default-features",
                "--features",
                "native-find,native-src",
            ])
            .output()
            .expect("failed to spawn nested cargo check");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "enabling BOTH native-find + native-src MUST fail the build; got success.\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("feature src and find are mutually exclusive"),
            "expected the upstream msquic build-script mutual-exclusion message; \
             the failure must originate upstream (not a crate-level guard).\nstderr:\n{stderr}"
        );
    }
}
