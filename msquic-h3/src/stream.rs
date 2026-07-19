//! HTTP/3 stream adapters: the [`H3Stream`]/[`H3SendStream`]/[`H3RecvStream`]
//! bidirectional/send/recv types plus the send/receive command-executor seams,
//! receive backpressure budget, per-stream callback context, and the native
//! `stream_callback` trampoline. Extracted verbatim from the crate root; the
//! public trait impls (`h3::quic::{SendStream, RecvStream, BidiStream}`) and the
//! three public `H3*Stream` types preserve their crate-root paths via re-export.

use std::{
    ffi::c_void,
    sync::{Arc, Mutex, MutexGuard},
};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
    ready,
};
use h3::quic::{BidiStream, RecvStream, SendStream, StreamErrorIncoming};
use msquic::{
    BufferRef, ConnectionShutdownFlags, ReceiveFlags, SendFlags, Status, StatusCode, StreamEvent,
    StreamOpenFlags, StreamRef, StreamShutdownFlags, StreamStartFlags,
};

use crate::buffer::{SendBuffer, SendLen, classify_send_len, copy_into_send_buffer};
use crate::config::DEFAULT_MAX_RECV_UNITS;
use crate::error::{
    ConnectionTerminal, ReceiveTerminal, SendCommand, SendEvent, SendInput, SendPayload, SendPoll,
    SendState, SendTerminal, clamp_application_code, convert_recv, convert_send, convert_send_op,
    transition,
};
use crate::{
    CbClass, ConnHandle, ConnTerminalSlot, ForceShutdown, H3_INTERNAL_ERROR, H3Config, PoisonFlag,
    SendTerminalSlot, ShutdownSeam, classify_conn_shutdown, commit_conn, guard_callback,
    internal_error_status, load_winner, lock_recover, new_send_terminal_slot, observe_conn_winner,
    peek_conn_terminal, publish_send, record_conn_terminal, report_contained_panic,
};

#[cfg(test)]
use crate::new_conn_terminal_slot;

/// Msquic Stream.
#[derive(Debug)]
pub struct H3Stream {
    pub(crate) send: H3SendStream,
    pub(crate) recv: H3RecvStream,
}
#[derive(Debug)]
pub struct H3SendStream {
    /// Every native send-side verb goes through `exec`; the production
    /// [`StreamExecutor`] (held here) owns the `Arc<msquic::Stream>` that keeps
    /// the send half's native handle alive. This ownership is independent of the
    /// recv half (which holds its own `Arc` clone), and it lets a test inject a
    /// double without a live connection.
    exec: Box<dyn SendExec>,
    pub(crate) sctx: SendStreamReceiveCtx,
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
pub(crate) trait SendExec: std::fmt::Debug + Send + Sync {
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
pub(crate) fn submit_owned_send(
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
pub(crate) trait RecvExec: std::fmt::Debug + Send + Sync {
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
pub(crate) trait OpenExec: std::fmt::Debug + Send + Sync {
    fn submit_open_start(&self, conn: &ConnHandle, uni: bool) -> Result<OpeningStream, Status>;
    /// `Connection::shutdown(NONE)` for `OpenStreams::close`, with an
    /// already-clamped application code. Behind the seam so a test double can
    /// assert the *actual* submitted value without a live connection.
    fn submit_conn_shutdown(&self, conn: &ConnHandle, code: u64);
}

/// Production open seam: the real `Stream::open` + `Stream::start`.
#[derive(Debug)]
pub(crate) struct StreamOpenExecutor;

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
    pub(crate) fn with_exec(exec: Box<dyn SendExec>, sctx: SendStreamReceiveCtx) -> Self {
        H3SendStream { exec, sctx }
    }
}

#[cfg(test)]
impl H3RecvStream {
    /// Test constructor: inject any [`RecvExec`] (the clamp-recording double) plus
    /// a receive-side context. No live stream is required — `stop_sending` touches
    /// only `exec` and `rctx`.
    pub(crate) fn with_exec(exec: Box<dyn RecvExec>, rctx: RecvStreamReceiveCtx) -> Self {
        H3RecvStream { exec, rctx }
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
pub(crate) const MAX_RECV_BUFFER: usize = 1024 * 1024; // 1 MiB per stream

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
/// owns its own independent budget sourced from the connection's [`H3Config`];
/// there is no connection-wide meter. See [`RecvState`].
///
/// `max_bytes` is the per-stream byte ceiling (default [`MAX_RECV_BUFFER`]).
/// `max_units` is the per-stream buffered-unit ceiling (default
/// [`DEFAULT_MAX_RECV_UNITS`]); it is carried here from Phase 1 but not yet
/// enforced — Phase 2 adds the unit-count backpressure that reads it.
#[derive(Debug)]
pub(crate) struct RecvBudget {
    state: Mutex<RecvState>,
    /// Per-stream buffered-byte ceiling (config-sourced).
    max_bytes: usize,
    /// Per-stream buffered-unit ceiling (config-sourced). Plumbed in Phase 1;
    /// enforced in Phase 2.
    #[allow(dead_code)]
    max_units: usize,
}

impl Default for RecvBudget {
    fn default() -> Self {
        Self::new(MAX_RECV_BUFFER, DEFAULT_MAX_RECV_UNITS)
    }
}

/// Outcome of the callback-side [`RecvBudget::admit`] transition.
pub(crate) enum Admit {
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
    /// Build a receive budget with explicit per-stream caps (from [`H3Config`]).
    pub(crate) fn new(max_bytes: usize, max_units: usize) -> Self {
        Self {
            state: Mutex::new(RecvState::default()),
            max_bytes,
            max_units,
        }
    }

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
        let pend = st.buffered >= self.max_bytes;
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
        if st.pend_outstanding && st.buffered < self.max_bytes {
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
    /// Config-sourced per-stream byte ceiling (threading assertion).
    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }
    /// Config-sourced per-stream unit ceiling (threading assertion; Phase 2
    /// enforces it).
    pub(crate) fn max_units(&self) -> usize {
        self.max_units
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
pub(crate) enum ReceiveEvent {
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

pub(crate) struct StreamSendCtx {
    start: Option<oneshot::Sender<Result<u64, Status>>>,
    /// Single ordered send-event channel (MF-1): data `SendComplete`, terminal
    /// wakes, and finish completion all ride this one FIFO so chronological
    /// finish-vs-terminal order is preserved by construction.
    pub(crate) send: Option<mpsc::UnboundedSender<SendEvent>>,
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
    pub(crate) poisoned: bool,
}

/// ctx for receiving data on frontend.
#[derive(Debug)]
pub(crate) struct RecvStreamReceiveCtx {
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
pub(crate) struct SendStreamReceiveCtx {
    /// Cached, validated stream identity (see [`RecvStreamReceiveCtx::id`]); read
    /// by `send_id` without a native query.
    id: h3::quic::StreamId,
    /// Shared sticky send-terminal slot (see [`SendTerminalSlot`]). Loaded on
    /// every reducer input as the current winner; local candidates are published
    /// here via the first-writer helper.
    pub(crate) send_terminal: SendTerminalSlot,
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
    pub(crate) send: mpsc::UnboundedReceiver<SendEvent>,
    /// Per-stream `send_data` payload ceiling (config-sourced, default
    /// [`MAX_ADAPTER_SEND`]). Read by `send_data` to classify/reject oversized
    /// payloads before any allocation.
    ///
    /// [`MAX_ADAPTER_SEND`]: crate::error::MAX_ADAPTER_SEND
    pub(crate) max_send_bytes: u64,
    /// Pure send-side reducer bookkeeping (replaces the old `send_inprogress`).
    pub(crate) reducer: SendState,
}

/// Frontend *receiver* ends held by an [`OpeningStream`] until the stream ID is
/// known. The matching *sender* halves live in the callback-owned
/// [`StreamSendCtx`]; holding these keeps every channel open (so callback sends
/// never fail) and lets `finalize` move them into the [`H3Stream`] halves.
#[derive(Debug)]
pub(crate) struct PreIdReceivers {
    pub(crate) start: oneshot::Receiver<Result<u64, Status>>,
    pub(crate) send: mpsc::UnboundedReceiver<SendEvent>,
    /// Shared send-terminal slot clone, moved into the send half by `finalize`.
    pub(crate) send_terminal: SendTerminalSlot,
    /// Shared connection terminal slot clone, moved into BOTH halves by
    /// `finalize` so each observation point can resolve+freeze the same winner.
    pub(crate) conn_terminal: ConnTerminalSlot,
    pub(crate) receive: mpsc::UnboundedReceiver<ReceiveEvent>,
    /// Per-stream receive budget (SF-A), carried into the receive half's ctx.
    pub(crate) recv_budget: Arc<RecvBudget>,
    /// Per-stream `send_data` ceiling (config-sourced), moved into the send half.
    pub(crate) max_send_bytes: u64,
}
/// [`h3::quic::StreamId`] is not yet known. Private to [`StreamOpener`]; the
/// only stream form allowed to exist without a cached ID. On a successful
/// `StartComplete` it is consumed into an [`H3Stream`] with concrete ID fields
/// in both halves.
#[derive(Debug)]
pub(crate) struct OpeningStream {
    pub(crate) stream: Arc<msquic::Stream>,
    /// Pending-start receiver, polled by `poll_open_inner` until `StartComplete`.
    pub(crate) start: oneshot::Receiver<Result<u64, Status>>,
    /// The remaining receiver ends, moved into the `H3Stream` halves by
    /// `finalize` once the ID is validated.
    pub(crate) tail: PreIdTail,
}

/// The non-start receiver ends of an [`OpeningStream`] (kept apart so `start` can
/// be polled independently while these wait for `finalize`).
#[derive(Debug)]
pub(crate) struct PreIdTail {
    pub(crate) send: mpsc::UnboundedReceiver<SendEvent>,
    pub(crate) send_terminal: SendTerminalSlot,
    /// Shared connection terminal slot clone, split into both halves' ctxs by
    /// `finalize`.
    pub(crate) conn_terminal: ConnTerminalSlot,
    pub(crate) receive: mpsc::UnboundedReceiver<ReceiveEvent>,
    /// Per-stream receive budget (SF-A), carried into the receive half's ctx.
    pub(crate) recv_budget: Arc<RecvBudget>,
    /// Per-stream `send_data` ceiling (config-sourced), moved into the send half.
    pub(crate) max_send_bytes: u64,
}

impl OpeningStream {
    /// Consume into an [`H3Stream`] once `StartComplete` has yielded a validated
    /// ID. The `start` receiver has already been driven to completion and is
    /// dropped here; the remaining three receivers move into the two halves.
    pub(crate) fn finalize(self, id: h3::quic::StreamId) -> H3Stream {
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
            max_send_bytes,
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
                    max_send_bytes,
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
pub(crate) fn stream_ctx_channel_pre_id(
    conn_terminal: ConnTerminalSlot,
    config: H3Config,
) -> (StreamSendCtx, PreIdReceivers) {
    let (start_tx, start_rx) = oneshot::channel::<Result<u64, Status>>();
    let (send_tx, send_rx) = mpsc::unbounded();
    let send_terminal = new_send_terminal_slot();
    let (receive_tx, receive_rx) = mpsc::unbounded();
    let recv_budget = Arc::new(RecvBudget::new(
        config.max_recv_bytes(),
        config.max_recv_units(),
    ));
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
            max_send_bytes: config.max_send_bytes(),
        },
    )
}

/// ID-bearing stream channel builder used where the identity is already known
/// (accepted streams and tests). Splits the pre-ID receivers into the two
/// frontend halves' ctxs directly. Creates a fresh, standalone connection
/// terminal slot and uses the default [`H3Config`]; tests that need to drive
/// connection-terminal refinement use [`stream_ctx_channel_with_conn`], and
/// tests that need custom caps use [`stream_ctx_channel_with_config`].
#[cfg(test)]
pub(crate) fn stream_ctx_channel(
    id: h3::quic::StreamId,
) -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    stream_ctx_channel_with_conn(id, new_conn_terminal_slot())
}

/// ID-bearing stream channel builder sharing an explicit connection terminal
/// slot with the caller, so a test can record/refine the connection terminal and
/// observe how the stream halves resolve+freeze it (FR-013). Uses the default
/// [`H3Config`].
#[cfg(test)]
pub(crate) fn stream_ctx_channel_with_conn(
    id: h3::quic::StreamId,
    conn_terminal: ConnTerminalSlot,
) -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    stream_ctx_channel_with_config(id, conn_terminal, H3Config::default())
}

/// ID-bearing stream channel builder with an explicit [`H3Config`], so a test
/// can assert the connection's caps thread into both stream halves (the
/// `RecvBudget` byte/unit caps and the send ceiling).
#[cfg(test)]
pub(crate) fn stream_ctx_channel_with_config(
    id: h3::quic::StreamId,
    conn_terminal: ConnTerminalSlot,
    config: H3Config,
) -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    let (ctx, recv) = stream_ctx_channel_pre_id(conn_terminal, config);
    let PreIdReceivers {
        start: _,
        send,
        send_terminal,
        conn_terminal,
        receive,
        recv_budget,
        max_send_bytes,
    } = recv;
    (
        ctx,
        SendStreamReceiveCtx {
            id,
            send_terminal,
            conn_terminal: conn_terminal.clone(),
            send,
            max_send_bytes,
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
pub(crate) fn stream_callback(ctx: &mut StreamSendCtx, ev: StreamEvent) -> Result<(), Status> {
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
pub(crate) fn stream_poison_disp(ev: &StreamEvent) -> Result<(), Status> {
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
pub(crate) fn stream_recover(
    ctx: &mut StreamSendCtx,
    seam: &dyn ShutdownSeam,
) -> Result<(), Status> {
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
    /// frontends do (FR-013). `config` is the accepting connection's memory
    /// budget, so the accepted stream inherits the same caps as a locally-opened
    /// one (FR-009).
    pub(crate) fn attach(
        stream: msquic::Stream,
        id: h3::quic::StreamId,
        conn_terminal: ConnTerminalSlot,
        config: H3Config,
    ) -> Self {
        let (mut ctx, recv) = stream_ctx_channel_pre_id(conn_terminal, config);
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
                max_send_bytes: recv.max_send_bytes,
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
        let (mut ctx, recv) = stream_ctx_channel_pre_id(conn.terminal().clone(), conn.config());
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
                max_send_bytes: recv.max_send_bytes,
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
        let payload = classify_payload(wb.remaining(), self.sctx.max_send_bytes);
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
/// allocation. The `NonEmpty` upper bound is the configured `ceiling`
/// (`<= u32::MAX` by [`H3Config`] validation), so the `as u32` cast never
/// truncates.
fn classify_payload(remaining: usize, ceiling: u64) -> SendPayload {
    match classify_send_len(remaining, ceiling) {
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

/// Phase 1 (config foundation & threading) unit tests: prove a connection's
/// [`H3Config`] caps reach BOTH stream halves via the same seam
/// (`stream_ctx_channel*`) that `H3Stream::attach` (peer-accepted) and
/// `OpeningStream::finalize` (locally-opened) funnel through, that distinct
/// per-connection configs stay isolated, and that the defaults equal the
/// historical constants (behavior unchanged). Hermetic (no network).
#[cfg(test)]
mod config_threading {
    use crate::error::MAX_ADAPTER_SEND;
    use crate::{H3Config, MAX_RECV_BUFFER, new_conn_terminal_slot};

    use super::stream_ctx_channel_with_config;

    fn custom_config() -> H3Config {
        H3Config::builder()
            .with_max_send_bytes(4096)
            .with_max_recv_bytes(8192)
            .with_max_recv_units(7)
            .build()
            .unwrap()
    }

    /// A custom config threads into a stream ctx built via the seam (the exact
    /// path both a locally-opened and a peer-accepted stream take through
    /// `stream_ctx_channel_pre_id`): the receive byte/unit caps and the send
    /// ceiling all equal the configured values.
    #[test]
    fn custom_config_threads_recv_and_send_caps() {
        let cfg = custom_config();
        let (_ctx, sctx, rctx) =
            stream_ctx_channel_with_config(4u64.try_into().unwrap(), new_conn_terminal_slot(), cfg);
        assert_eq!(rctx.recv_budget.max_bytes(), 8192);
        assert_eq!(rctx.recv_budget.max_units(), 7);
        assert_eq!(sctx.max_send_bytes, 4096);
    }

    /// The same custom config reaching an ACCEPTED-shaped stream (distinct
    /// conn-terminal slot, mimicking `H3Stream::attach`'s dedicated seam call)
    /// carries the identical caps: peer-accepted streams inherit the connection
    /// config (FR-009).
    #[test]
    fn custom_config_threads_to_accepted_shaped_stream() {
        let cfg = custom_config();
        let accepted_slot = new_conn_terminal_slot();
        let (_ctx, sctx, rctx) =
            stream_ctx_channel_with_config(6u64.try_into().unwrap(), accepted_slot, cfg);
        assert_eq!(rctx.recv_budget.max_bytes(), 8192);
        assert_eq!(rctx.recv_budget.max_units(), 7);
        assert_eq!(sctx.max_send_bytes, 4096);
    }

    /// Default-config construction yields caps equal to the historical constants
    /// (no behavioral change).
    #[test]
    fn default_config_threads_the_historical_constants() {
        let (_ctx, sctx, rctx) = stream_ctx_channel_with_config(
            4u64.try_into().unwrap(),
            new_conn_terminal_slot(),
            H3Config::default(),
        );
        assert_eq!(rctx.recv_budget.max_bytes(), MAX_RECV_BUFFER);
        assert_eq!(rctx.recv_budget.max_units(), 16384);
        assert_eq!(sctx.max_send_bytes, MAX_ADAPTER_SEND);
    }

    /// Two connections with different configs produce streams with their own
    /// respective caps — no cross-talk (Spec per-connection independence).
    #[test]
    fn two_configs_are_isolated() {
        let a = H3Config::builder()
            .with_max_send_bytes(1024)
            .with_max_recv_bytes(2048)
            .with_max_recv_units(3)
            .build()
            .unwrap();
        let b = H3Config::builder()
            .with_max_send_bytes(9999)
            .with_max_recv_bytes(55555)
            .with_max_recv_units(99)
            .build()
            .unwrap();
        let (_ca, sa, ra) =
            stream_ctx_channel_with_config(4u64.try_into().unwrap(), new_conn_terminal_slot(), a);
        let (_cb, sb, rb) =
            stream_ctx_channel_with_config(8u64.try_into().unwrap(), new_conn_terminal_slot(), b);

        assert_eq!(ra.recv_budget.max_bytes(), 2048);
        assert_eq!(ra.recv_budget.max_units(), 3);
        assert_eq!(sa.max_send_bytes, 1024);

        assert_eq!(rb.recv_budget.max_bytes(), 55555);
        assert_eq!(rb.recv_budget.max_units(), 99);
        assert_eq!(sb.max_send_bytes, 9999);
    }
}
