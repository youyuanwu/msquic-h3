//! The client/accepted QUIC connection (Group D): the public [`Connection`]
//! type, its FFI callback + panic-recovery seam, the accept frontends, and the
//! [`ConnectionShutdownWaiter`].
//!
//! [`Connection`] owns the native handle via [`ConnHandle`] and a
//! [`ConnCtxReceiver`] frontend fed by [`connection_callback`]. The callback
//! records terminal reasons into the shared terminal slot, resolves the connect
//! one-shot, validates + delivers peer-accepted streams, and — under
//! [`guard_callback`] — never unwinds across the FFI boundary. The two public
//! `h3::quic` trait impls (`Connection` accept + `OpenStreams` open) live here;
//! the [`StreamOpener`] they delegate to is defined in the crate root.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use bytes::Buf;
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    ready,
};
use h3::quic::{ConnectionErrorIncoming, OpenStreams, StreamErrorIncoming};
use msquic::{
    Configuration, ConnectionEvent, ConnectionRef, ConnectionShutdownFlags, Status, StatusCode,
    StreamOpenFlags,
};

use crate::error::{ConnectionTerminal, convert_conn};
use crate::registration::{Registration, RundownGuard};
use crate::{
    CbClass, ConnTerminalSlot, ConnTerminalState, ForceShutdown, H3_INTERNAL_ERROR, H3RecvStream,
    H3SendStream, H3Stream, PoisonFlag, ShutdownSeam, StreamOpener, classify_transport,
    commit_conn, guard_callback, internal_error_status, lock_recover, record_conn_terminal,
    report_contained_panic,
};

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
    pub(crate) conn: Arc<ConnHandle>,
    pub(crate) ctx: ConnCtxReceiver,
    pub(crate) opener: StreamOpener,
}

/// from callback send to fount end.
#[derive(Debug)]
pub(crate) struct ConnCtxSender {
    pub(crate) connected: Option<oneshot::Sender<Result<(), Status>>>,
    shutdown: Option<oneshot::Sender<()>>,
    pub(crate) bidi: Option<mpsc::UnboundedSender<Option<crate::H3Stream>>>,
    pub(crate) uni: Option<mpsc::UnboundedSender<Option<crate::H3Stream>>>,
    /// Shared connection terminal-reason slot (writer side).
    pub(crate) terminal: ConnTerminalSlot,
    /// Set by [`connection_recover`] after a contained callback panic (SF-E), so
    /// [`guard_callback`] short-circuits every subsequent event on this ctx.
    pub(crate) poisoned: bool,
    /// Connection-scoped test seam that forces an accepted-stream ID resolution
    /// to fail exactly once. Shares its atomic with
    /// [`ConnCtxReceiver::accepted_id_failpoint`] (created once in
    /// [`conn_ctx_channel`]) so a live [`Connection`] can arm it before an
    /// accept; lives in the shared connection state rather than a
    /// thread-local/process-global because msquic callbacks run on worker
    /// threads and a connection can migrate between them.
    #[cfg(test)]
    pub(crate) accepted_id_failpoint: Arc<AcceptedIdFailpoint>,
}

/// front end receive.
#[derive(Debug)]
pub(crate) struct ConnCtxReceiver {
    pub(crate) connected: Option<oneshot::Receiver<Result<(), Status>>>,
    shutdown: Option<oneshot::Receiver<()>>,
    bidi: mpsc::UnboundedReceiver<Option<crate::H3Stream>>,
    uni: mpsc::UnboundedReceiver<Option<crate::H3Stream>>,
    /// Shared connection terminal-reason slot (reader side).
    pub(crate) terminal: ConnTerminalSlot,
    /// Reader/frontend handle to the connection-scoped accepted-ID failpoint.
    /// Shares the same atomic as [`ConnCtxSender::accepted_id_failpoint`], so a
    /// test holding a live [`Connection`] (which owns this receiver) can arm the
    /// failpoint *before* a peer stream is accepted; the callback's
    /// `PeerStreamStarted` path then reads it through the sender-side clone.
    #[cfg(test)]
    pub(crate) accepted_id_failpoint: Arc<AcceptedIdFailpoint>,
}

pub(crate) fn conn_ctx_channel() -> (ConnCtxSender, ConnCtxReceiver) {
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
pub(crate) fn validate_stream_id(
    raw: u64,
) -> Result<h3::quic::StreamId, h3::quic::InvalidStreamId> {
    h3::quic::StreamId::try_from(raw)
}

/// Wake both accept frontends by dropping the incoming-stream senders.
///
/// Used only on the fail-fast accepted-stream attachment path: once the adapter
/// has declared its own connection state invalid (an `Internal` terminal), it
/// must stop handing new streams to h3 and unpark any parked acceptor so it
/// observes the published terminal.
pub(crate) fn wake_acceptors(ctx: &mut ConnCtxSender) {
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
pub(crate) fn accept_stream_id(
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
pub(crate) struct AcceptedIdFailpoint {
    mode: std::sync::atomic::AtomicU8,
}

#[cfg(test)]
impl AcceptedIdFailpoint {
    const OFF: u8 = 0;
    const QUERY_FAIL: u8 = 1;
    const INVALID_ID: u8 = 2;

    /// Arm the next accepted-stream attachment to fail its native ID query.
    pub(crate) fn arm_query_fail(&self) {
        self.mode
            .store(Self::QUERY_FAIL, std::sync::atomic::Ordering::Relaxed);
    }

    /// Arm the next accepted-stream attachment to yield an invalid h3 ID.
    pub(crate) fn arm_invalid_id(&self) {
        self.mode
            .store(Self::INVALID_ID, std::sync::atomic::Ordering::Relaxed);
    }

    /// Consume the armed mode atomically and transform the queried raw ID:
    /// off → `Ok(raw)`, query-fail → `Err(status)`, invalid → an out-of-range
    /// ID that fails [`validate_stream_id`].
    pub(crate) fn maybe_override(&self, raw: u64) -> Result<u64, Status> {
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
pub(crate) fn connection_callback(
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
pub(crate) fn conn_poison_disp(ev: &ConnectionEvent) -> Result<(), Status> {
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
pub(crate) fn connection_recover(
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
pub(crate) fn fail_fast_terminal(slot: &ConnTerminalSlot) -> Option<ConnectionErrorIncoming> {
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
pub(crate) fn observe_terminal(slot: &ConnTerminalSlot) -> ConnectionErrorIncoming {
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
