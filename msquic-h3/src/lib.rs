use std::{
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
    Configuration, ConnectionEvent, ConnectionRef, ConnectionShutdownFlags, ReceiveFlags,
    SendFlags, Status, StatusCode, StreamEvent, StreamOpenFlags, StreamRef, StreamShutdownFlags,
    StreamStartFlags,
};

mod buffer;
use buffer::{SendBuffer, SendLen, classify_send_len};
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

/// Shared, thread-safe slot recording *why* a connection terminated.
///
/// Shared between the connection FFI callback (the writer) and the accept
/// frontends / stream opener (readers). All access goes through
/// [`lock_recover`] so an FFI callback never panics on a poisoned lock.
pub(crate) type ConnTerminalSlot = Arc<Mutex<ConnTerminalState>>;

/// Create a fresh, empty connection terminal-reason slot. Test-only helper for
/// exercising the terminal slot directly (production builds the slot inline in
/// [`conn_ctx_channel`]).
#[cfg(test)]
pub(crate) fn new_conn_terminal_slot() -> ConnTerminalSlot {
    Arc::new(Mutex::new(ConnTerminalState::default()))
}

/// First-writer-wins record of the connection terminal reason, with
/// provisional-to-specific refinement until the value is externally observed.
///
/// A provisional cause (`LocalClose`) recorded first may be refined by a later,
/// more-specific peer/transport cause — but only until an external observer
/// (an accept frontend delivering the terminal to h3) has frozen the value.
/// After the freeze point the winner is immutable. This is the connection-scope
/// SF-7 / T4 rule; it is deliberately independent of the send-scope cancellation
/// state (MF-2), which lives in the send-terminal slot (as an unobserved
/// [`SendTerminal::ProvisionalAbort`] marker) and is never written here.
#[derive(Debug, Default)]
pub(crate) struct ConnTerminalState {
    terminal: Option<ConnectionTerminal>,
    observed: bool,
}

impl ConnTerminalState {
    /// Record a candidate terminal reason under the first-writer / refinement
    /// rule. A no-op once the value has been frozen by [`Self::observe`].
    fn record(&mut self, candidate: ConnectionTerminal) {
        if self.observed {
            return;
        }
        match &self.terminal {
            None => self.terminal = Some(candidate),
            Some(existing) => {
                // Only a provisional cause may be refined, and only to a
                // specific one. Specific causes never regress to provisional.
                if existing.is_provisional() && !candidate.is_provisional() {
                    self.terminal = Some(candidate);
                }
            }
        }
    }

    /// Freeze the slot and return a clone of the recorded reason (if any).
    ///
    /// After this call the value is immutable: later [`Self::record`] calls are
    /// ignored. This is the external-observation point of the refinement rule.
    fn observe(&mut self) -> Option<ConnectionTerminal> {
        self.observed = true;
        self.terminal.clone()
    }

    /// Whether an internal (fail-fast) terminal has been published. Read before
    /// draining queued streams so an internal failure is reported immediately
    /// rather than behind already-queued items.
    fn has_internal(&self) -> bool {
        matches!(self.terminal, Some(ConnectionTerminal::Internal(_)))
    }
}

/// Record a connection terminal reason into the shared slot.
fn record_conn_terminal(slot: &ConnTerminalSlot, candidate: ConnectionTerminal) {
    lock_recover(slot).record(candidate);
}

/// Shared, thread-safe slot recording the sticky *why* a send half terminated.
///
/// Written by the stream FFI callback (peer `STOP_SENDING`, connection shutdown)
/// and by the send-side reducer's local candidates (immediate send/graceful/reset
/// failure, `LocalReset`, internal faults). A single first-writer owns the slot,
/// except the distinct unobserved provisional cancellation marker
/// ([`SendTerminal::ProvisionalAbort`], MF-2), which is refined to a later specific
/// cause or finalized to an authoritative abort at the closure point. Both the
/// callback and the send frontend hold a clone, so a peer/connection terminal is
/// observable from `poll_ready`/`poll_finish` even with no send in flight. Never
/// written into the connection terminal slot (the provisional stays send-scoped).
pub(crate) type SendTerminalSlot = Arc<Mutex<Option<SendTerminal>>>;

/// Create a fresh, empty send-terminal slot.
fn new_send_terminal_slot() -> SendTerminalSlot {
    Arc::new(Mutex::new(None))
}

/// Read the current sticky send winner (poison-safe clone; `None` if unset).
fn load_winner(slot: &SendTerminalSlot) -> Option<SendTerminal> {
    lock_recover(slot).clone()
}

/// First-writer publish of a local send-terminal candidate, returning whichever
/// value now owns the slot.
///
/// The distinct provisional cancellation marker ([`SendTerminal::ProvisionalAbort`],
/// MF-2) already in the slot is refined by a later authoritative candidate (a
/// specific peer/connection cause, a `LocalReset`, or the closure-point abort); an
/// authoritative winner is never overwritten. The callback and the reducer race
/// into the same slot; whichever authoritative cause lands first wins, and the
/// unobserved provisional marker never permanently masks it.
fn publish_send(slot: &SendTerminalSlot, candidate: SendTerminal) -> SendTerminal {
    let mut guard = lock_recover(slot);
    match &*guard {
        // Empty slot: this candidate wins. Return it directly (no re-clone/unwrap).
        None => {
            *guard = Some(candidate.clone());
            candidate
        }
        Some(existing) => {
            if existing.is_provisional() && !candidate.is_provisional() {
                // Refine the unobserved provisional marker to the authoritative cause.
                *guard = Some(candidate.clone());
                candidate
            } else {
                // First authoritative writer (or a provisional that cannot refine an
                // existing value) keeps the slot; surface the current owner.
                existing.clone()
            }
        }
    }
}

/// Classify a transport shutdown status into a connection terminal reason.
///
/// Idle and connection timeouts map to [`ConnectionTerminal::Timeout`]; every
/// other transport status is retained verbatim (status + wire error code) as a
/// [`ConnectionTerminal::Transport`] for `Undefined` mapping at the boundary.
fn classify_transport(status: Status, error_code: u64) -> ConnectionTerminal {
    match status.try_as_status_code() {
        Ok(StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT)
        | Ok(StatusCode::QUIC_STATUS_CONNECTION_IDLE) => ConnectionTerminal::Timeout,
        _ => ConnectionTerminal::Transport { status, error_code },
    }
}

/// Classify a connection-caused stream `ShutdownComplete` into a connection
/// terminal reason.
///
/// This is the deterministic fallback a *stream* callback uses when it observes
/// `ShutdownComplete { connection_shutdown: true, .. }`. It follows MsQuic's
/// `ConnectionShutdownByApp` / `ConnectionClosedRemotely` semantics (see
/// `docs/error-propagation.md`, "Receive-side transitions"):
/// - `by_app && closed_remotely` is a peer HTTP/3 application close;
/// - `by_app && !closed_remotely` is a local application close (no peer code);
/// - anything else is a transport close, delegated to [`classify_transport`]
///   (which distinguishes idle/handshake timeouts from generic transport).
fn classify_conn_shutdown(
    by_app: bool,
    closed_remotely: bool,
    error_code: u64,
    status: Status,
) -> ConnectionTerminal {
    match (by_app, closed_remotely) {
        (true, true) => ConnectionTerminal::PeerApplication(error_code),
        (true, false) => ConnectionTerminal::LocalClose,
        (false, _) => classify_transport(status, error_code),
    }
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

/// Non-freezing read of the connection terminal reason.
///
/// Unlike [`ConnTerminalState::observe`], this clones the recorded reason
/// without marking the slot observed, so the stream-open path can consult the
/// connection terminal without freezing it for the accept frontends.
fn peek_conn_terminal(slot: &ConnTerminalSlot) -> Option<ConnectionTerminal> {
    lock_recover(slot).terminal.clone()
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
fn connection_callback(ctx: &mut ConnCtxSender, ev: msquic::ConnectionEvent) -> Result<(), Status> {
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
            let id = match accept_stream_id(ctx, query) {
                Ok(id) => id,
                // Pre-ownership failure: `accept_stream_id` already published the
                // internal terminal and woke the acceptors. Return the status so
                // msquic closes the rejected stream itself (single close).
                Err(status) => return Err(status),
            };
            // Success: only NOW take native ownership and attach.
            let owned = unsafe { msquic::Stream::from_raw(stream.as_raw()) };
            let h3 = crate::H3Stream::attach(owned, id);
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

impl Connection {
    /// Connects to the server.
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
            let handler =
                move |_: ConnectionRef, ev: ConnectionEvent| connection_callback(&mut ctx, ev);
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
        let handler =
            move |_: ConnectionRef, ev: ConnectionEvent| connection_callback(&mut ctx, ev);
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

/// Freeze the terminal slot and convert the recorded reason for h3.
///
/// Called when the incoming-stream channel is drained/closed. A closure with no
/// recorded reason maps to a defined internal error rather than a synthetic
/// application close.
fn observe_terminal(slot: &ConnTerminalSlot) -> ConnectionErrorIncoming {
    match lock_recover(slot).observe() {
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
        // 1. Fail fast if the connection already published a terminal.
        if let Some(reason) = peek_conn_terminal(conn.terminal()) {
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
                    // A shutdown may have raced the open; prefer the terminal.
                    return Poll::Ready(Err(match peek_conn_terminal(conn.terminal()) {
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
fn stream_open_conn_error(slot: &ConnTerminalSlot) -> ConnectionErrorIncoming {
    match peek_conn_terminal(slot) {
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
/// `Status` as `Unknown`. A successful start validates the ID; an out-of-range
/// ID is an adapter-internal fault (never `Unknown`).
fn classify_start_outcome(
    raw: Result<u64, Status>,
    slot: &ConnTerminalSlot,
) -> Result<h3::quic::StreamId, StreamErrorIncoming> {
    match raw {
        Err(status) => Err(match peek_conn_terminal(slot) {
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

impl SendExec for StreamExecutor {
    fn submit_send(&mut self, buf: SendBuffer) -> Result<(), Status> {
        // Box the self-referential SendBuffer and hand its raw pointer to MsQuic
        // as `client_context`. `buf`'s single `BufferRef` points into the owned
        // `Bytes` heap storage and stays valid because the box is not moved again
        // until the `SendComplete` callback (or the immediate-failure reclaim
        // below) reconstructs it.
        let cc: *mut SendBuffer = Box::into_raw(Box::new(buf));
        // SAFETY: `cc` is a live `Box<SendBuffer>` leaked just above; `(*cc)` is a
        // unique, valid reference. Its `buffers` memory stays valid until the
        // `SendComplete` callback reconstructs and drops the box (`Stream::send`'s
        // documented contract). FIN is not set on a data send — a graceful finish
        // is a separate `submit_graceful` shutdown call.
        let res = unsafe {
            self.stream
                .send((*cc).buffers(), SendFlags::NONE, cc as *const c_void)
        };
        if let Err(status) = res {
            // Immediate failure: MsQuic took no ownership and delivers no
            // `SendComplete`, so reclaim the box here (mirroring the callback drop)
            // before returning. Reclaimed exactly once.
            // SAFETY: `cc` is the pointer from `Box::into_raw` above; MsQuic did
            // not take ownership on an error, so this is the sole reclamation.
            drop(unsafe { Box::from_raw(cc) });
            return Err(status);
        }
        Ok(())
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
    receive: Option<mpsc::UnboundedSender<ReceiveEvent>>,
    /// First-writer-wins guard for the receive scope: once a `Fin`, `Reset`, or
    /// `Connection` terminal has been published, later receive events are
    /// suppressed (e.g. the usual `Receive { FIN }` then `PeerSendShutdown`
    /// sequence must not enqueue a duplicate FIN).
    receive_terminal_sent: bool,
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
    /// SF-6: local, sticky end-of-stream flag set by OUR OWN `stop_sending`.
    /// Distinct from `terminal` (a peer/connection-caused reason); when set,
    /// `poll_data` returns a clean `Ok(None)` and `terminal` is left untouched.
    receive_closed: bool,
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
    receive: mpsc::UnboundedReceiver<ReceiveEvent>,
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
    receive: mpsc::UnboundedReceiver<ReceiveEvent>,
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
            receive,
        } = tail;
        H3Stream {
            send: H3SendStream {
                exec: Box::new(StreamExecutor {
                    stream: stream.clone(),
                }),
                sctx: SendStreamReceiveCtx {
                    id,
                    send_terminal,
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
                    receive_closed: false,
                },
            },
        }
    }
}

/// Pre-ID variant of the stream channel builder: no [`h3::quic::StreamId`] is
/// required or produced. Builds the callback-owned [`StreamSendCtx`] (senders)
/// and returns the matching receiver ends bundled, so an [`OpeningStream`] can
/// hold them until `StartComplete` yields the ID.
fn stream_ctx_channel_pre_id() -> (StreamSendCtx, PreIdReceivers) {
    let (start_tx, start_rx) = oneshot::channel::<Result<u64, Status>>();
    let (send_tx, send_rx) = mpsc::unbounded();
    let send_terminal = new_send_terminal_slot();
    let (receive_tx, receive_rx) = mpsc::unbounded();
    (
        StreamSendCtx {
            start: Some(start_tx),
            send: Some(send_tx),
            send_terminal: send_terminal.clone(),
            receive: Some(receive_tx),
            receive_terminal_sent: false,
        },
        PreIdReceivers {
            start: start_rx,
            send: send_rx,
            send_terminal,
            receive: receive_rx,
        },
    )
}

/// ID-bearing stream channel builder used where the identity is already known
/// (accepted streams and tests). Splits the pre-ID receivers into the two
/// frontend halves' ctxs directly.
#[cfg(test)]
fn stream_ctx_channel(
    id: h3::quic::StreamId,
) -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    let (ctx, recv) = stream_ctx_channel_pre_id();
    let PreIdReceivers {
        start: _,
        send,
        send_terminal,
        receive,
    } = recv;
    (
        ctx,
        SendStreamReceiveCtx {
            id,
            send_terminal,
            send,
            reducer: SendState::new(),
        },
        RecvStreamReceiveCtx {
            id,
            receive,
            terminal: None,
            receive_closed: false,
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
            // returns that same pointer here exactly once. A null pointer means no
            // owned buffer was attached (e.g. a test-synthesized event), so it is
            // skipped rather than dereferenced.
            if let Some(cc) = std::ptr::NonNull::new(client_context as *mut SendBuffer) {
                drop(unsafe { Box::from_raw(cc.as_ptr()) });
            }
            if let Some(send) = ctx.send.as_ref() {
                // The buffer is already reclaimed above; only the outcome flag is
                // forwarded to the reducer via the single ordered channel (MF-1).
                // A gone frontend is a safe no-op.
                let _ = send.unbounded_send(SendEvent::Complete { cancelled });
            }
        }
        StreamEvent::Receive { buffers, flags, .. } => {
            // Enqueue received bytes (if any) BEFORE any terminal marker from
            // this same notification, so `poll_data` observes data then FIN.
            // Once a receive terminal has been published, suppress further data.
            if !ctx.receive_terminal_sent
                && let Some(receive) = ctx.receive.as_ref()
            {
                let mut b = BytesMut::new();
                for br in buffers {
                    // skip empty buffs.
                    if !br.as_bytes().is_empty() {
                        b.put_slice(br.as_bytes());
                    }
                }
                let b = b.freeze();
                if !b.is_empty() {
                    // Failed delivery (frontend `H3RecvStream` dropped) drops
                    // the sender so no further receive events are attempted.
                    if receive.unbounded_send(ReceiveEvent::Data(b)).is_err() {
                        ctx.receive.take();
                    }
                }
                // An empty, non-FIN notification produces NO event (Finding
                // 8): a zero-length receive is not by itself an end-of-stream.
            }
            // A FIN flag is a clean end-of-stream marker (peer finished sending).
            if flags.contains(ReceiveFlags::FIN) {
                publish_recv_terminal(ctx, ReceiveEvent::Fin);
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
            // on the receive half (reusing the Phase-3 connection terminal +
            // convert helpers) and, symmetrically, publishes it as the sticky
            // send terminal so the send half observes the same reason. A
            // stream-local shutdown publishes no terminal.
            if connection_shutdown {
                let reason = classify_conn_shutdown(
                    connection_shutdown_by_app,
                    connection_closed_remotely,
                    connection_error_code,
                    connection_close_status,
                );
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
    pub(crate) fn attach(stream: msquic::Stream, id: h3::quic::StreamId) -> Self {
        let (mut ctx, recv) = stream_ctx_channel_pre_id();
        let handler = move |_: StreamRef, ev: StreamEvent| stream_callback(&mut ctx, ev);
        stream.set_callback_handler(handler);
        // The ID is known, so finalize straight away. An accepted stream never
        // receives `StartComplete`, so its `start` receiver simply drops.
        OpeningStream {
            stream: Arc::new(stream),
            start: recv.start,
            tail: PreIdTail {
                send: recv.send,
                send_terminal: recv.send_terminal,
                receive: recv.receive,
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
    fn open_and_start(conn: &msquic::Connection, uni: bool) -> Result<OpeningStream, Status> {
        let (mut ctx, recv) = stream_ctx_channel_pre_id();
        let handler = move |_: StreamRef, ev: StreamEvent| stream_callback(&mut ctx, ev);

        let flag = match uni {
            true => StreamOpenFlags::UNIDIRECTIONAL,
            false => StreamOpenFlags::NONE,
        };

        let s = msquic::Stream::open(conn, flag, handler)?;
        s.start(StreamStartFlags::NONE)?; // id arrives later via StartComplete
        Ok(OpeningStream {
            stream: Arc::new(s),
            start: recv.start,
            tail: PreIdTail {
                send: recv.send,
                send_terminal: recv.send_terminal,
                receive: recv.receive,
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
            return match load_winner(&self.sctx.send_terminal) {
                Some(w) => Poll::Ready(Err(convert_send(w))),
                None => Poll::Ready(Err(convert_send(SendTerminal::Internal(
                    "poll_ready after finish",
                )))),
            };
        }
        // First input polls the channel; subsequent inputs are fed results.
        let mut input = SendInput::PollReady {
            poll: self.poll_send_channel(cx),
            terminal: load_winner(&self.sctx.send_terminal),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::Pending => return Poll::Pending, // waker already registered
                SendCommand::ReturnReady => return Poll::Ready(Ok(())),
                SendCommand::ReturnError(t) => return Poll::Ready(Err(convert_send(t))),
                SendCommand::RepollReady => {
                    // Re-poll the channel in THIS call so a synchronously-queued
                    // paired terminal / channel close (the closure point) is drained
                    // or the waker re-armed — the retained provisional cancellation
                    // (MF-2) is never returned to the caller.
                    input = SendInput::PollReady {
                        poll: self.poll_send_channel(cx),
                        terminal: load_winner(&self.sctx.send_terminal),
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
            terminal: load_winner(&self.sctx.send_terminal),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::ReturnSent => return Ok(()),
                SendCommand::ReturnImmediateError(e) => return Err(convert_send_op(e)),
                SendCommand::ReturnError(t) => return Err(convert_send(t)),
                SendCommand::SubmitSend => {
                    // The single production copy path: materialize the complete
                    // wire representation into owned `Bytes` while `B` is still on
                    // this thread, then hand it to the seam (which reclaims the box
                    // itself on an immediate `Err`).
                    let remaining = wb.remaining();
                    let buf = SendBuffer::new(wb.copy_to_bytes(remaining));
                    let res = self.exec.submit_send(buf);
                    input = SendInput::SendSubmitted {
                        result: res,
                        terminal: load_winner(&self.sctx.send_terminal),
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
            terminal: load_winner(&self.sctx.send_terminal),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::Pending => return Poll::Pending,
                SendCommand::ReturnFinished => return Poll::Ready(Ok(())),
                SendCommand::ReturnError(t) => return Poll::Ready(Err(convert_send(t))),
                SendCommand::SubmitGraceful => {
                    let res = self.exec.submit_graceful();
                    input = SendInput::GracefulSubmitted {
                        result: res,
                        terminal: load_winner(&self.sctx.send_terminal),
                    };
                }
                SendCommand::RepollFinish => {
                    // Re-poll the channel in THIS call so a synchronously-queued
                    // `FinishComplete` is drained (or the waker re-armed) — never
                    // defer to a later poll (that recreates the lost-waker bug).
                    input = SendInput::PollFinish {
                        poll: self.poll_send_channel(cx),
                        terminal: load_winner(&self.sctx.send_terminal),
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
        let mut input = SendInput::Reset {
            code: clamp_application_code(reset_code),
            terminal: load_winner(&self.sctx.send_terminal),
        };
        loop {
            match transition(&mut self.sctx.reducer, input) {
                SendCommand::NoOp => return, // already terminal / published
                SendCommand::SubmitReset(c) => {
                    let res = self.exec.submit_reset(c);
                    input = SendInput::ResetSubmitted {
                        code: c,
                        result: res,
                        terminal: load_winner(&self.sctx.send_terminal),
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
            Some(ReceiveEvent::Connection(reason)) => {
                self.store_and_convert(ReceiveTerminal::Connection(reason))
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
        self.rctx.poll_event(cx)
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

    #[test]
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

/// Phase 1 (callback safety) unit tests: proves the FFI callback surfaces are
/// panic-free when a receiver has been dropped, and that the poison-recovering
/// lock helper never panics on a poisoned mutex. Hermetic (no network).
#[cfg(test)]
mod callback_safety {
    use std::sync::{Arc, Mutex};

    use crate::msquic::{ConnectionEvent, StreamEvent};
    use crate::{
        conn_ctx_channel, connection_callback, lock_recover, stream_callback, stream_ctx_channel,
    };

    #[test]
    fn connection_callback_connected_with_dropped_receiver_is_noop() {
        let (mut ctx, rx) = conn_ctx_channel();
        // Frontend gone: the `Connected` one-shot receiver is dropped.
        drop(rx);
        let ev = ConnectionEvent::Connected {
            session_resumed: false,
            negotiated_alpn: &[],
        };
        // The fallible send returns Err internally; the callback must not panic
        // and must return Ok across the FFI boundary.
        assert!(connection_callback(&mut ctx, ev).is_ok());
        // The one-shot slot was consumed exactly once.
        assert!(ctx.connected.is_none());
    }

    #[test]
    fn stream_callback_send_complete_with_dropped_receiver_is_noop() {
        let (mut ctx, srx, rrx) = stream_ctx_channel(4u64.try_into().unwrap());
        // Frontend gone: drop the receive side of the send-complete channel.
        drop(srx);
        drop(rrx);
        let ev = StreamEvent::SendComplete {
            cancelled: false,
            client_context: std::ptr::null(),
        };
        // Fallible unbounded_send returns Err; no panic, callback returns Ok.
        assert!(stream_callback(&mut ctx, ev).is_ok());
    }

    #[test]
    fn lock_recover_recovers_poisoned_mutex() {
        let m = Arc::new(Mutex::new(41u32));
        let m2 = m.clone();
        // Poison the mutex by panicking while its guard is held.
        let joined = std::thread::spawn(move || {
            let mut g = m2.lock().unwrap();
            *g = 42;
            panic!("poison the callback-path lock");
        })
        .join();
        assert!(joined.is_err(), "helper thread should have panicked");
        assert!(m.is_poisoned(), "mutex should be poisoned");

        // Recover without panicking and observe the value written before poison.
        let g = lock_recover(&m);
        assert_eq!(*g, 42);
    }
}

/// Phase 3 (connection terminal slot & incoming-terminal propagation) unit
/// tests: proves the connection close mapping, the connected one-shot carrying
/// `Result<(), Status>`, the provisional-to-specific refinement/freeze rule, and
/// the drain-vs-fail-fast queue policy. Hermetic (no network).
#[cfg(test)]
mod connection_terminal {
    use h3::quic::ConnectionErrorIncoming;

    use crate::error::ConnectionTerminal;
    use crate::msquic::{ConnectionEvent, Status, StatusCode};
    use crate::{
        ConnCtxReceiver, ConnTerminalState, classify_transport, conn_ctx_channel,
        connection_callback, fail_fast_terminal, new_conn_terminal_slot, observe_terminal,
        record_conn_terminal,
    };

    /// Drive one connection event through the callback and return the frozen,
    /// converted terminal the accept frontend would report.
    fn map_event(ev: ConnectionEvent) -> ConnectionErrorIncoming {
        let (mut ctx, crx) = conn_ctx_channel();
        assert!(connection_callback(&mut ctx, ev).is_ok());
        observe_terminal(&crx.terminal)
    }

    #[test]
    fn peer_application_close_maps_to_application_close_with_code() {
        let err = map_event(ConnectionEvent::ShutdownInitiatedByPeer { error_code: 42 });
        assert!(
            matches!(
                err,
                ConnectionErrorIncoming::ApplicationClose { error_code: 42 }
            ),
            "expected ApplicationClose(42), got {err:?}"
        );
    }

    #[test]
    fn idle_timeout_maps_to_timeout() {
        let ev = ConnectionEvent::ShutdownInitiatedByTransport {
            status: Status::new(StatusCode::QUIC_STATUS_CONNECTION_IDLE),
            error_code: 0,
        };
        let err = map_event(ev);
        assert!(
            matches!(err, ConnectionErrorIncoming::Timeout),
            "expected Timeout, got {err:?}"
        );
    }

    #[test]
    fn connection_timeout_maps_to_timeout() {
        let ev = ConnectionEvent::ShutdownInitiatedByTransport {
            status: Status::new(StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT),
            error_code: 0,
        };
        assert!(matches!(map_event(ev), ConnectionErrorIncoming::Timeout));
    }

    #[test]
    fn other_transport_failure_maps_to_undefined_transport_error() {
        let ev = ConnectionEvent::ShutdownInitiatedByTransport {
            status: Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR),
            error_code: 7,
        };
        let err = map_event(ev);
        match err {
            ConnectionErrorIncoming::Undefined(e) => {
                // The adapter-owned transport error retains the wire code.
                let s = e.to_string();
                assert!(s.contains("transport error_code 7"), "display was: {s}");
            }
            other => panic!("expected Undefined(MsQuicTransportError), got {other:?}"),
        }
    }

    #[test]
    fn local_close_maps_to_undefined_local_close() {
        // A bare ShutdownComplete with no more-specific reason is a local close.
        let err = map_event(ConnectionEvent::ShutdownComplete {
            handshake_completed: false,
            peer_acknowledged_shutdown: false,
            app_close_in_progress: false,
        });
        match err {
            ConnectionErrorIncoming::Undefined(e) => {
                assert!(e.to_string().contains("locally"), "display was: {e}");
            }
            other => panic!("expected Undefined(LocalConnectionClose), got {other:?}"),
        }
    }

    #[test]
    fn channel_closed_without_reason_maps_to_internal_error() {
        // No callback fired: the slot is empty when the channel drains.
        let slot = new_conn_terminal_slot();
        match observe_terminal(&slot) {
            ConnectionErrorIncoming::InternalError(msg) => {
                assert!(msg.contains("without a terminal reason"), "msg: {msg}");
            }
            other => panic!("expected InternalError, got {other:?}"),
        }
    }

    /// Read the resolved value of the connect one-shot without blocking. Panics
    /// if the waiter is still pending (would hang) or was cancelled.
    fn connect_result(crx: &mut ConnCtxReceiver) -> Result<(), Status> {
        crx.connected
            .take()
            .expect("connected waiter present")
            .try_recv()
            .expect("connect waiter resolved, not cancelled (no hang)")
            .expect("connect waiter produced a value, not Pending")
    }

    #[test]
    fn connected_resolves_ok() {
        let (mut ctx, mut crx) = conn_ctx_channel();
        let ev = ConnectionEvent::Connected {
            session_resumed: false,
            negotiated_alpn: &[],
        };
        assert!(connection_callback(&mut ctx, ev).is_ok());
        assert!(connect_result(&mut crx).is_ok());
    }

    #[test]
    fn connected_waiter_resolves_with_transport_status_on_early_shutdown() {
        let (mut ctx, mut crx) = conn_ctx_channel();
        // Transport shutdown before Connected carries the real status.
        let ev = ConnectionEvent::ShutdownInitiatedByTransport {
            status: Status::new(StatusCode::QUIC_STATUS_HANDSHAKE_FAILURE),
            error_code: 0,
        };
        assert!(connection_callback(&mut ctx, ev).is_ok());
        let err = connect_result(&mut crx).expect_err("early shutdown resolves as Err");
        assert_eq!(
            err.try_as_status_code().ok(),
            Some(StatusCode::QUIC_STATUS_HANDSHAKE_FAILURE),
            "connect() should surface the real transport cause, not synthetic ABORTED"
        );
    }

    #[test]
    fn connected_waiter_resolves_on_peer_shutdown_before_connected() {
        let (mut ctx, mut crx) = conn_ctx_channel();
        assert!(
            connection_callback(
                &mut ctx,
                ConnectionEvent::ShutdownInitiatedByPeer { error_code: 9 }
            )
            .is_ok()
        );
        // Peer close has no lossless status; the waiter resolves (no hang) as
        // ABORTED, while the exact peer code stays in the terminal slot.
        let err = connect_result(&mut crx).expect_err("peer shutdown resolves as Err");
        assert_eq!(
            err.try_as_status_code().ok(),
            Some(StatusCode::QUIC_STATUS_ABORTED)
        );
        assert!(matches!(
            observe_terminal(&crx.terminal),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }

    #[test]
    fn connected_waiter_resolves_on_bare_shutdown_complete() {
        let (mut ctx, mut crx) = conn_ctx_channel();
        assert!(
            connection_callback(
                &mut ctx,
                ConnectionEvent::ShutdownComplete {
                    handshake_completed: false,
                    peer_acknowledged_shutdown: false,
                    app_close_in_progress: false,
                }
            )
            .is_ok()
        );
        let err = connect_result(&mut crx).expect_err("bare shutdown resolves as Err");
        assert_eq!(
            err.try_as_status_code().ok(),
            Some(StatusCode::QUIC_STATUS_ABORTED)
        );
    }

    #[test]
    fn classify_transport_table() {
        assert!(matches!(
            classify_transport(Status::new(StatusCode::QUIC_STATUS_CONNECTION_IDLE), 0),
            ConnectionTerminal::Timeout
        ));
        assert!(matches!(
            classify_transport(Status::new(StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT), 0),
            ConnectionTerminal::Timeout
        ));
        assert!(matches!(
            classify_transport(Status::new(StatusCode::QUIC_STATUS_TLS_ERROR), 3),
            ConnectionTerminal::Transport { error_code: 3, .. }
        ));
    }

    #[test]
    fn provisional_local_close_refines_to_specific_before_observation() {
        let mut st = ConnTerminalState::default();
        st.record(ConnectionTerminal::LocalClose);
        // A more-specific peer cause published before observation wins.
        st.record(ConnectionTerminal::PeerApplication(7));
        let t = st.observe().expect("terminal recorded");
        assert!(matches!(t, ConnectionTerminal::PeerApplication(7)));
        // Frozen after observation: later records are ignored.
        st.record(ConnectionTerminal::Timeout);
        assert!(matches!(
            st.observe(),
            Some(ConnectionTerminal::PeerApplication(7))
        ));
    }

    #[test]
    fn specific_cause_does_not_regress_to_provisional() {
        let mut st = ConnTerminalState::default();
        st.record(ConnectionTerminal::Timeout);
        // A later provisional local close must not overwrite the specific cause.
        st.record(ConnectionTerminal::LocalClose);
        assert!(matches!(st.observe(), Some(ConnectionTerminal::Timeout)));
    }

    #[test]
    fn first_specific_cause_wins_over_later_specific() {
        let mut st = ConnTerminalState::default();
        st.record(ConnectionTerminal::PeerApplication(1));
        st.record(ConnectionTerminal::Timeout);
        assert!(matches!(
            st.observe(),
            Some(ConnectionTerminal::PeerApplication(1))
        ));
    }

    #[test]
    fn refinement_frozen_by_observation_even_for_provisional() {
        let mut st = ConnTerminalState::default();
        st.record(ConnectionTerminal::LocalClose);
        // Observing freezes the provisional value; a later specific cause that
        // arrives after the frontend already reported cannot change it.
        assert!(matches!(st.observe(), Some(ConnectionTerminal::LocalClose)));
        st.record(ConnectionTerminal::PeerApplication(3));
        assert!(matches!(st.observe(), Some(ConnectionTerminal::LocalClose)));
    }

    #[test]
    fn normal_shutdown_drains_before_reporting_terminal() {
        // A non-internal terminal does not fail fast: the accept frontend keeps
        // the drain-then-terminal ordering (queued streams first).
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(5));
        assert!(
            fail_fast_terminal(&slot).is_none(),
            "a normal peer close must not fail fast ahead of queued streams"
        );
        // Once the channel drains, the recorded reason is reported.
        assert!(matches!(
            observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 5 }
        ));
    }

    #[test]
    fn internal_terminal_fails_fast_ahead_of_queued_streams() {
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::Internal("boom"));
        match fail_fast_terminal(&slot) {
            Some(ConnectionErrorIncoming::InternalError(msg)) => assert_eq!(msg, "boom"),
            other => panic!("expected fail-fast InternalError, got {other:?}"),
        }
    }

    #[test]
    fn callback_records_local_close_provisionally_then_refines() {
        // Simulate OpenStreams::close (records LocalClose) racing a peer cause
        // that lands before the frontend observes: the specific cause wins.
        let (mut ctx, crx) = conn_ctx_channel();
        record_conn_terminal(&ctx.terminal, ConnectionTerminal::LocalClose);
        assert!(
            connection_callback(
                &mut ctx,
                ConnectionEvent::ShutdownInitiatedByPeer { error_code: 11 }
            )
            .is_ok()
        );
        assert!(matches!(
            observe_terminal(&crx.terminal),
            ConnectionErrorIncoming::ApplicationClose { error_code: 11 }
        ));
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
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use h3::quic::{ConnectionErrorIncoming, StreamErrorIncoming};

    use crate::msquic::{BufferRef, ReceiveFlags, Status, StatusCode, StreamEvent};
    use crate::{RecvStreamReceiveCtx, stream_callback, stream_ctx_channel};

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
    use futures::channel::oneshot;
    use h3::quic::{ConnectionErrorIncoming, StreamErrorIncoming};

    use crate::error::ConnectionTerminal;
    use crate::msquic::{Status, StatusCode};
    use crate::{
        accept_stream_id, classify_start_outcome, conn_ctx_channel, fail_fast_terminal,
        new_conn_terminal_slot, record_conn_terminal, stream_ctx_channel_pre_id,
        stream_open_conn_error, validate_stream_id,
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
    fn start_cancellation_is_detected_without_panic() {
        // Simulate ShutdownComplete dropping the start sender (StreamSendCtx)
        // before StartComplete: the OpeningStream's start receiver resolves as
        // Canceled, which poll_open_inner maps to a connection error (never a
        // panic). Here we assert the Canceled detection + the mapping helper.
        let (ctx, mut recv) = stream_ctx_channel_pre_id();
        drop(ctx); // drops the start sender -> cancellation
        match recv.start.try_recv() {
            Err(oneshot::Canceled) => {}
            other => panic!("expected Canceled after sender drop, got {other:?}"),
        }
        // Cancelled with no published reason -> internal error.
        let slot = new_conn_terminal_slot();
        assert!(matches!(
            stream_open_conn_error(&slot),
            ConnectionErrorIncoming::InternalError(_)
        ));
        // Cancelled by a connection close -> connection error.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(3));
        assert!(matches!(
            stream_open_conn_error(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 3 }
        ));
    }

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

    use crate::error::MAX_ADAPTER_SEND;
    use crate::msquic::{Status, StatusCode, StreamEvent};
    use crate::{
        ConnectionErrorIncoming, H3SendStream, OversizedSend, SendBuffer, SendExec,
        SendStreamReceiveCtx, StreamSendCtx, stream_callback, stream_ctx_channel,
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

    /// Test double for the send seam. Mirrors `StreamExecutor`'s box/into_raw and
    /// immediate-failure reclaim EXACTLY, so a scripted result models the real
    /// allocation and ownership handoff without a native stream.
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
            // Mirror StreamExecutor EXACTLY: box + into_raw BEFORE inspecting the
            // scripted result, so both arms model the real allocation.
            let cc = Box::into_raw(Box::new(buf));
            self.h.allocs.fetch_add(1, Relaxed);
            match self.script.pop_front().unwrap_or(Ok(())) {
                Ok(()) => {
                    // Native accepted ownership: retain the pointer so the test can
                    // replay the native SendComplete through stream_callback. The
                    // box is NOT dropped here; it is outstanding until that callback.
                    self.h
                        .client_ctx
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push_back(cc as usize);
                    Ok(())
                }
                Err(status) => {
                    // Immediate failure: MsQuic takes no ownership and delivers no
                    // SendComplete, so the caller reclaims here and now — exactly as
                    // StreamExecutor's immediate-Err arm does.
                    // SAFETY: `cc` is the pointer from `Box::into_raw` above; this is
                    // its sole reclamation (native took no ownership on Err).
                    drop(unsafe { Box::from_raw(cc) });
                    self.h.reclaims.fetch_add(1, Relaxed);
                    Err(status)
                }
            }
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
    use h3::quic::{OpenStreams, RecvStream};

    use crate::error::clamp_application_code;
    use crate::msquic::{Connection, ConnectionEvent, ConnectionRef, RegistrationConfig, Status};
    use crate::registration::RundownGuard;
    use crate::{
        ConnHandle, H3RecvStream, OpenExec, OpeningStream, RecvExec, Registration, StreamOpener,
        new_conn_terminal_slot, stream_ctx_channel,
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
}
