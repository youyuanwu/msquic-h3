//! Error vocabulary for the msquic <-> h3 adapter.
//!
//! This module introduces the shared, adapter-owned error types, the scoped
//! terminal enums that record *why* a connection, receive half, or send half
//! ended, and the pure conversion helpers that mint fresh `h3` error values at
//! the polling boundary. It also defines the outgoing application-code clamp.
//!
//! Most of the terminal enums and conversion helpers are wired into the FFI
//! callbacks and polling paths in later phases (Phase 3 onwards); they are
//! introduced here so the vocabulary exists in one place. The `#[allow(dead_code)]`
//! attributes below are scoped to individual not-yet-wired items and each notes
//! the phase that consumes it.

use std::fmt;

use h3::quic::{ConnectionErrorIncoming, StreamErrorIncoming};

use crate::msquic::{Status, StatusCode};

/// Maximum value representable by a QUIC 62-bit variable-length integer.
///
/// Outgoing application error codes must never exceed this or msquic rejects the
/// encode. See [`clamp_application_code`].
pub const MAX_QUIC_VARINT: u64 = (1 << 62) - 1;

/// Default maximum single `send_data` payload the adapter will accept before
/// rejecting with [`OversizedSend`] (16 MiB). Configurable via [`H3Config`](crate::H3Config).
pub const MAX_ADAPTER_SEND: u64 = 16 * 1024 * 1024;

/// Clamp an outgoing application error code to the QUIC 62-bit varint maximum.
///
/// `(1 << 62) - 1` passes unchanged; `1 << 62` and above clamp down to the
/// maximum. Applied at every site where an application code crosses the FFI
/// boundary (connection close, `stop_sending`, and — in Phase 7 — `reset`).
pub(crate) fn clamp_application_code(code: u64) -> u64 {
    code.min(MAX_QUIC_VARINT)
}

// ---------------------------------------------------------------------------
// Adapter-owned error types.
//
// Each is `Debug + Display + Error + Send + Sync` so it can be `Arc`'d into
// `ConnectionErrorIncoming::Undefined(Arc<dyn Error + Send + Sync>)` or boxed
// into `StreamErrorIncoming::Unknown(Box<dyn Error + Send + Sync>)`.
// ---------------------------------------------------------------------------

/// A `send_data` payload above `MAX_ADAPTER_SEND`. One-shot; never stored in
/// the shared send-terminal slot. Constructed at the `send_data` rejection site
/// and boxed into `StreamErrorIncoming::Unknown`.
#[derive(Debug)]
pub struct OversizedSend {
    /// Requested payload length in bytes (the `remaining()` that was rejected).
    pub len: usize,
    /// The connection's configured send-size ceiling that was exceeded.
    pub max_bytes: u64,
}

impl fmt::Display for OversizedSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "send_data payload of {} bytes exceeds configured ceiling ({} bytes)",
            self.len, self.max_bytes
        )
    }
}

impl std::error::Error for OversizedSend {}

/// A non-application transport shutdown. Both the QUIC status and the wire
/// transport error code are retained for diagnostics; the transport code is a
/// QUIC transport code, not an HTTP/3 application code.
#[derive(Debug)]
pub struct MsQuicTransportError {
    /// The QUIC status that accompanied the transport shutdown.
    pub status: Status,
    /// The wire transport error code (diagnostic only).
    pub error_code: u64,
}

impl fmt::Display for MsQuicTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QUIC transport shutdown: status {} (transport error_code {})",
            self.status, self.error_code
        )
    }
}

impl std::error::Error for MsQuicTransportError {}

/// A local (application-initiated) connection close. Carries no peer code: the
/// required mapping is explicit that we must not invent one.
#[derive(Debug)]
pub struct LocalConnectionClose;

impl fmt::Display for LocalConnectionClose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connection closed locally by the application")
    }
}

impl std::error::Error for LocalConnectionClose {}

/// A local reset via `SendStream::reset(code)`. Distinct from a peer
/// `StreamTerminated`: h3 should not poll a reset send stream, and if it does we
/// surface this rather than pretending the peer terminated the stream.
#[derive(Debug)]
pub struct LocalStreamReset {
    /// The (already-clamped) local reset code.
    pub code: u64,
}

impl fmt::Display for LocalStreamReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "send stream reset locally with code {}", self.code)
    }
}

impl std::error::Error for LocalStreamReset {}

// ---------------------------------------------------------------------------
// Scoped terminal enums.
//
// Small, adapter-owned reasons recorded in callback-facing state. `Clone` where
// multiple consumers need the reason. Converted to fresh h3 error values only at
// the polling boundary via the `convert_*` helpers below.
// ---------------------------------------------------------------------------

/// Why a connection terminated. The first writer wins for the connection scope,
/// except a *provisional* cause may be refined to a more specific peer/transport
/// cause before the terminal is externally observed (see
/// [`ConnectionTerminal::is_provisional`]).
#[derive(Clone, Debug)]
pub(crate) enum ConnectionTerminal {
    /// Peer closed with an HTTP/3 application error code.
    PeerApplication(u64),
    /// Idle or handshake timeout.
    Timeout,
    /// Non-application transport shutdown.
    Transport {
        /// The QUIC status that accompanied the shutdown.
        status: Status,
        /// The wire transport error code (diagnostic only).
        error_code: u64,
    },
    /// Local application-initiated close.
    LocalClose,
    /// Internal adapter failure.
    #[allow(dead_code)] // constructed by the accepted-stream fail-fast path in Phase 5
    Internal(&'static str),
}

impl ConnectionTerminal {
    /// Whether this cause is *provisional* and may still be refined.
    ///
    /// A provisional cause (a local close, or — in later phases — a generic
    /// transport fallback derived without a specific reason) may be replaced by
    /// a subsequently-published, more-specific peer/transport cause until the
    /// terminal has been externally observed. Specific peer/transport causes and
    /// internal failures are authoritative and never refine to a provisional
    /// value. See "Terminal-cause refinement" in `docs/error-model.md`.
    pub(crate) fn is_provisional(&self) -> bool {
        matches!(self, ConnectionTerminal::LocalClose)
    }
}

/// Why a receive half terminated. The first writer wins for the receive scope.
#[derive(Clone, Debug)]
pub(crate) enum ReceiveTerminal {
    /// Clean end of stream (peer FIN / send-shutdown).
    Fin,
    /// Peer reset the stream with the given code.
    Reset(u64),
    /// The whole connection terminated.
    Connection(ConnectionTerminal),
    /// Internal adapter failure.
    Internal(&'static str),
}

/// Why a send half terminated. The first writer wins for the send scope, except
/// the distinct *provisional* cancellation marker ([`SendTerminal::ProvisionalAbort`],
/// synthesized for a cancelled `SendComplete` with no yet-known cause, MF-2)
/// which may be refined to a more-specific peer/connection cause — or finalized
/// to an authoritative [`SendTerminal::Failed`] abort at the closure point —
/// while it is still unobserved.
#[derive(Clone, Debug)]
pub(crate) enum SendTerminal {
    /// Peer sent `STOP_SENDING` with the given code.
    Stopped(u64),
    /// The whole connection terminated.
    Connection(ConnectionTerminal),
    /// Local `reset(code)` (already clamped). Not a peer termination.
    LocalReset(u64),
    /// A native send/start failure carrying its status. Authoritative: even a
    /// native `QUIC_STATUS_ABORTED` here is a *real* failure, never refinable.
    Failed(Status),
    /// A *provisional*, still-refinable cancellation synthesized for a cancelled
    /// `SendComplete` that arrived with no published peer/connection cause (MF-2).
    /// It is deliberately distinct from a real [`SendTerminal::Failed`]: only this
    /// marker is refinable, it is never surfaced to the caller (it stays
    /// unobserved), and it is finalized to `Failed(QUIC_STATUS_ABORTED)` at the
    /// defined closure point when no richer cause has appeared.
    ProvisionalAbort,
    /// Internal adapter failure.
    Internal(&'static str),
}

impl SendTerminal {
    /// Whether this cause is the *provisional* send cancellation marker that may
    /// still be refined to a more-specific cause (MF-2).
    ///
    /// Only [`SendTerminal::ProvisionalAbort`] is provisional. A specific peer
    /// `Stopped`, a `Connection` reason, a `LocalReset`, an internal failure, or
    /// **any** `Failed` status (including a real native `QUIC_STATUS_ABORTED`) is
    /// authoritative and never refined. See "Terminal-cause refinement" in `docs/error-model.md`.
    pub(crate) fn is_provisional(&self) -> bool {
        matches!(self, SendTerminal::ProvisionalAbort)
    }
}

/// The synthesized status for an unclassified send cancellation.
pub(crate) fn aborted_status() -> Status {
    Status::new(StatusCode::QUIC_STATUS_ABORTED)
}

// ---------------------------------------------------------------------------
// Conversion helpers.
//
// These are the *only* places that mint h3 error values from adapter terminals.
// Pure functions; wired into the polling boundary in later phases.
// ---------------------------------------------------------------------------

/// Convert a [`ConnectionTerminal`] into the h3 connection error it represents.
pub(crate) fn convert_conn(t: ConnectionTerminal) -> ConnectionErrorIncoming {
    use ConnectionTerminal as C;
    match t {
        C::PeerApplication(code) => ConnectionErrorIncoming::ApplicationClose { error_code: code },
        C::Timeout => ConnectionErrorIncoming::Timeout,
        C::Transport { status, error_code } => {
            ConnectionErrorIncoming::Undefined(std::sync::Arc::new(MsQuicTransportError {
                status,
                error_code,
            }))
        }
        C::LocalClose => {
            ConnectionErrorIncoming::Undefined(std::sync::Arc::new(LocalConnectionClose))
        }
        C::Internal(msg) => ConnectionErrorIncoming::InternalError(msg.to_string()),
    }
}

/// Convert a [`SendTerminal`] into the h3 stream error it represents.
pub(crate) fn convert_send(t: SendTerminal) -> StreamErrorIncoming {
    use SendTerminal as S;
    match t {
        S::Stopped(code) => StreamErrorIncoming::StreamTerminated { error_code: code },
        S::Connection(reason) => StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: convert_conn(reason),
        },
        S::LocalReset(code) => StreamErrorIncoming::Unknown(Box::new(LocalStreamReset { code })),
        S::Failed(status) => StreamErrorIncoming::Unknown(Box::new(status)),
        // A provisional marker is never surfaced to the caller while unobserved;
        // if it is ever converted (a defensive, non-panicking fallback) it maps to
        // the same authoritative aborted status its closure-point finalization uses.
        S::ProvisionalAbort => StreamErrorIncoming::Unknown(Box::new(aborted_status())),
        S::Internal(msg) => StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: ConnectionErrorIncoming::InternalError(msg.to_string()),
        },
    }
}

/// Convert a [`ReceiveTerminal`] into the h3 receive outcome it represents.
///
/// A clean FIN maps to `Ok(None)`; every other terminal maps to an error.
pub(crate) fn convert_recv(
    t: ReceiveTerminal,
) -> Result<Option<bytes::Bytes>, StreamErrorIncoming> {
    use ReceiveTerminal as R;
    match t {
        R::Fin => Ok(None),
        R::Reset(code) => Err(StreamErrorIncoming::StreamTerminated { error_code: code }),
        R::Connection(reason) => Err(StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: convert_conn(reason),
        }),
        R::Internal(msg) => Err(StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: ConnectionErrorIncoming::InternalError(msg.to_string()),
        }),
    }
}

// ---------------------------------------------------------------------------
// Send-side reducer (Phase 7).
//
// A pure, native-free state machine for the send half. It mutates only
// [`SendState`] and emits one [`SendCommand`] per input; the frontend executor
// loops (in `stream.rs`) run the returned command against MsQuic through the
// [`crate::SendExec`] seam and feed results straight back. Keeping the reducer
// pure makes every transition exhaustively table-testable with no native handle.
// See "Send-side transitions" / the reducer in `docs/error-model.md`.
// ---------------------------------------------------------------------------

/// One order-sensitive send event. All send events (data completion, terminal
/// wake, finish completion) ride a **single** mpsc so chronological finish-vs-
/// terminal order is preserved by construction (MF-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendEvent {
    /// A native `SendComplete` for an outstanding data send; `cancelled` is the
    /// native cancel flag.
    Complete { cancelled: bool },
    /// A terminal wake: a peer `STOP_SENDING` or connection shutdown published a
    /// sticky [`SendTerminal`] into the shared slot and woke the send half.
    TerminalWake,
    /// A `SendShutdownComplete`: the graceful (or aborted) finish completed.
    FinishComplete { graceful: bool },
}

/// Reducer bookkeeping for the send half. `*_submitting` are transaction
/// reservations set before a native call so a reentrant/repeated input cannot
/// emit a second submission; each clears when its matching `*Submitted` arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SendState {
    /// A data `SubmitSend` was issued; awaiting `SendSubmitted`.
    pub(crate) send_submitting: bool,
    /// A data send is committed and awaiting its `SendComplete`.
    pub(crate) send_inprogress: bool,
    /// A `SubmitGraceful` was issued; awaiting `GracefulSubmitted`.
    pub(crate) finish_submitting: bool,
    /// Graceful shutdown was submitted (committed on `GracefulSubmitted(Ok)`).
    pub(crate) finish_started: bool,
    /// The graceful finish completed (absorbing success).
    pub(crate) finish_complete: bool,
    /// A `SubmitReset` was issued; awaiting `ResetSubmitted`.
    pub(crate) reset_submitting: bool,
}

impl SendState {
    /// A fresh, all-false state.
    pub(crate) fn new() -> Self {
        SendState::default()
    }

    /// A native submission is reserved but its `*Submitted` result has not arrived.
    pub(crate) fn submission_reserved(&self) -> bool {
        self.send_submitting || self.finish_submitting || self.reset_submitting
    }
}

/// Which frontend method initiated a `PublishTerminal`, so the reducer can pick
/// the correct continuation once the first-writer winner is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalContinuation {
    SendData,
    PollReady,
    PollFinish,
    Reset,
}

/// Outcome of polling the single send-event channel. `Closed` (a channel closed
/// with no terminal item) is an adapter-internal fault, distinct from `Pending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendPoll {
    Pending,
    Event(SendEvent),
    Closed,
}

/// `send_data` payload classification, computed from `remaining()` before any
/// owned bytes are materialized. The `NonEmpty`/`Oversized` split is against
/// [`MAX_ADAPTER_SEND`], not the raw native `u32::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendPayload {
    Empty,
    NonEmpty { len: u32 },
    Oversized { len: usize, ceiling: u64 },
}

/// One-shot, non-sticky adapter error for `send_data`. Never stored in the
/// shared slot; a later `poll_ready` is unaffected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SendOperationError {
    OversizedSend { len: usize, ceiling: u64 },
    Misuse(&'static str),
}

/// Inputs to [`transition`]. Each `terminal` field is the current shared winner
/// the caller loaded from the send-terminal slot (poison-safe).
#[derive(Clone, Debug)]
pub(crate) enum SendInput {
    /// A `send_data` request; `payload` is classified before allocation.
    SendRequested {
        payload: SendPayload,
        terminal: Option<SendTerminal>,
    },
    /// Immediate result of the `SubmitSend` native call.
    SendSubmitted {
        result: Result<(), Status>,
        terminal: Option<SendTerminal>,
    },
    /// A `poll_ready` request, carrying the send-channel poll outcome.
    PollReady {
        poll: SendPoll,
        terminal: Option<SendTerminal>,
    },
    /// A `poll_finish` request, carrying the send-channel poll outcome.
    PollFinish {
        poll: SendPoll,
        terminal: Option<SendTerminal>,
    },
    /// A `reset(code)` request (code already clamped).
    Reset {
        code: u64,
        terminal: Option<SendTerminal>,
    },
    /// Immediate result of a `SubmitGraceful` native call.
    GracefulSubmitted {
        result: Result<(), Status>,
        terminal: Option<SendTerminal>,
    },
    /// Immediate result of a `SubmitReset` native call.
    ResetSubmitted {
        code: u64,
        result: Result<(), Status>,
        terminal: Option<SendTerminal>,
    },
    /// The actual winner returned by the first-writer publish helper, fed back
    /// after a `PublishTerminal` command, tagged with its initiating method.
    TerminalPublished {
        winner: SendTerminal,
        continuation: TerminalContinuation,
    },
}

/// Commands emitted by [`transition`]. The frontend executor loop runs each and
/// feeds the result back as a further [`SendInput`].
#[derive(Clone, Debug)]
pub(crate) enum SendCommand {
    /// `Poll::Pending`; the caller has already registered the waker.
    Pending,
    /// Infallible no-op (e.g. `reset` after a terminal).
    NoOp,
    /// `send_data` success / no-op (empty payload); no bytes submitted.
    ReturnSent,
    /// Construct the allocation and call `submit_send`; result -> `SendSubmitted`.
    SubmitSend,
    /// Call `submit_graceful`; result -> `GracefulSubmitted`.
    SubmitGraceful,
    /// Call `submit_reset(code)`; result -> `ResetSubmitted`.
    SubmitReset(u64),
    /// Re-poll the send channel and feed a fresh `PollFinish` in the SAME call,
    /// so a synchronously-queued `FinishComplete` is drained (or the waker is
    /// re-armed) before `Pending`.
    RepollFinish,
    /// Re-poll the send channel and feed a fresh `PollReady` in the SAME call.
    /// Emitted after retaining an unobserved provisional cancellation (MF-2): it
    /// drains a synchronously-queued paired terminal / channel close (the closure
    /// point) or re-arms the waker before `Pending`, so the provisional value is
    /// never returned to the caller.
    RepollReady,
    /// Publish a local terminal candidate via the first-writer helper; the
    /// resolved winner is fed back as `TerminalPublished { winner, continuation }`.
    PublishTerminal {
        candidate: SendTerminal,
        continuation: TerminalContinuation,
    },
    /// Return a one-shot, non-sticky `send_data` error (oversized / misuse).
    ReturnImmediateError(SendOperationError),
    /// `poll_ready` success (`Poll::Ready(Ok(()))`).
    ReturnReady,
    /// `poll_finish` success (`Poll::Ready(Ok(()))`).
    ReturnFinished,
    /// A terminal winner surfaced to the caller as an error.
    ReturnError(SendTerminal),
}

/// Convert a one-shot [`SendOperationError`] into the h3 error `send_data`
/// returns. Never stored in the shared slot.
pub(crate) fn convert_send_op(e: SendOperationError) -> StreamErrorIncoming {
    match e {
        SendOperationError::OversizedSend { len, ceiling } => {
            StreamErrorIncoming::Unknown(Box::new(OversizedSend {
                len,
                max_bytes: ceiling,
            }))
        }
        SendOperationError::Misuse(msg) => StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: ConnectionErrorIncoming::InternalError(msg.to_string()),
        },
    }
}

/// The winner only if it is an *authoritative* (non-provisional) cause. A
/// provisional cancellation marker ([`SendTerminal::ProvisionalAbort`], MF-2) is
/// treated as "no definitive winner yet": it is never surfaced to the caller and
/// never freezes the slot, so an authoritative local candidate (a specific cause,
/// a `LocalReset`, or the closure-point abort) refines it instead.
fn definitive(terminal: &Option<SendTerminal>) -> Option<SendTerminal> {
    terminal.clone().filter(|w| !w.is_provisional())
}

/// Whether an unobserved provisional cancellation marker is currently retained in
/// the shared slot (MF-2). Drives the closure-point decisions in the event arms.
fn provisional_pending(terminal: &Option<SendTerminal>) -> bool {
    terminal.as_ref().is_some_and(SendTerminal::is_provisional)
}

/// Pure, native-free send transition. Mutates only `state`; emits one command.
///
/// One exhaustive `match` in the normative precedence order: `*Submitted`
/// bookkeeping first (always clears its reservation before any winner), then
/// absorbing/queued finish, then method-specific terminal handling, then event
/// rows. Every impossible pairing is an explicit `Internal` publish, never a panic.
///
/// The `terminal` field of every input is a **non-freezing snapshot** of the
/// resolved shared-slot winner (the frontend's `resolve_terminal`); the reducer
/// consults it only through [`definitive`] / [`provisional_pending`], so a
/// provisional MF-2 cancellation marker is never observed by the caller — it is
/// refined by a paired terminal callback or finalized to an authoritative abort at
/// the closure point (channel close, finish/reset completion). The reducer never
/// freezes the shared connection slot: when it emits a [`SendCommand::ReturnError`]
/// carrying a connection cause, the frontend commits (freezes) that connection
/// identity via `commit_send_winner` at the exact delivery point, so the freeze
/// happens only when the cause is actually returned to h3 (commit-on-delivery).
pub(crate) fn transition(state: &mut SendState, input: SendInput) -> SendCommand {
    use SendCommand::*;
    use SendInput::*;
    use TerminalContinuation as K;
    let aborted = || SendTerminal::Failed(aborted_status());

    match input {
        // ── Precedence 1: *Submitted bookkeeping. Clears its reservation before
        //    any winner is applied; a missing reservation is internal.
        SendSubmitted { result, terminal } => {
            if !state.send_submitting {
                return PublishTerminal {
                    candidate: SendTerminal::Internal("SendSubmitted without reservation"),
                    continuation: K::SendData,
                };
            }
            state.send_submitting = false;
            match result {
                Ok(()) => {
                    state.send_inprogress = true;
                    match definitive(&terminal) {
                        Some(w) => ReturnError(w), // a specific winner raced in
                        None => ReturnSent,        // send_data's Ok(())
                    }
                }
                // Buffer already reclaimed by the caller on the Err path. A real
                // native failure is authoritative; it refines a provisional marker.
                Err(status) => PublishTerminal {
                    candidate: definitive(&terminal).unwrap_or(SendTerminal::Failed(status)),
                    continuation: K::SendData,
                },
            }
        }
        GracefulSubmitted { result, terminal } => {
            if !state.finish_submitting {
                return PublishTerminal {
                    candidate: SendTerminal::Internal("GracefulSubmitted without reservation"),
                    continuation: K::PollFinish,
                };
            }
            state.finish_submitting = false;
            match result {
                Ok(()) => {
                    state.finish_started = true;
                    RepollFinish // never Pending directly (drain a queued FinishComplete)
                }
                Err(status) => PublishTerminal {
                    candidate: definitive(&terminal).unwrap_or(SendTerminal::Failed(status)),
                    continuation: K::PollFinish,
                },
            }
        }
        ResetSubmitted {
            code,
            result,
            terminal,
        } => {
            if !state.reset_submitting {
                return PublishTerminal {
                    candidate: SendTerminal::Internal("ResetSubmitted without reservation"),
                    continuation: K::Reset,
                };
            }
            state.reset_submitting = false;
            // Reset completion is a closure point: an authoritative `LocalReset`
            // (or native failure) refines any retained provisional cancellation.
            let candidate = match result {
                Ok(()) => definitive(&terminal).unwrap_or(SendTerminal::LocalReset(code)),
                Err(status) => definitive(&terminal).unwrap_or(SendTerminal::Failed(status)),
            };
            PublishTerminal {
                candidate,
                continuation: K::Reset,
            }
        }

        // ── Consume the resolved winner (no state mutation).
        TerminalPublished {
            winner,
            continuation,
        } => {
            if winner.is_provisional() {
                // MF-2: an unobserved provisional cancellation is never returned to
                // the caller. Re-poll to drain a synchronously-queued paired terminal
                // / channel close (the closure point) or re-arm the waker.
                match continuation {
                    K::PollReady => RepollReady,
                    K::PollFinish => RepollFinish,
                    // Provisional markers never arise on the send_data / reset paths.
                    K::SendData | K::Reset => NoOp,
                }
            } else {
                match continuation {
                    K::SendData | K::PollReady | K::PollFinish => ReturnError(winner),
                    K::Reset => NoOp, // infallible method; winner stored for a later poll
                }
            }
        }

        // ── send_data request. Terminal wins; else misuse if busy/finishing.
        SendRequested { payload, terminal } => {
            if let Some(w) = definitive(&terminal) {
                return ReturnError(w);
            }
            if state.send_inprogress
                || state.send_submitting
                || state.finish_submitting
                || state.finish_started
                || state.finish_complete
            {
                return ReturnImmediateError(SendOperationError::Misuse(
                    "send_data called while a send is already in progress",
                ));
            }
            match payload {
                // idle, no winner
                SendPayload::Empty => ReturnSent, // no-op, no reservation
                SendPayload::Oversized { len, ceiling } => {
                    ReturnImmediateError(SendOperationError::OversizedSend { len, ceiling }) // state unchanged
                }
                SendPayload::NonEmpty { .. } => {
                    state.send_submitting = true;
                    SubmitSend
                }
            }
        }

        // ── reset request (infallible; at most one native SubmitReset).
        Reset { code, terminal } => {
            // A provisional cancellation does NOT block the reset: reset is a
            // closure point whose authoritative `LocalReset` refines it.
            if state.finish_complete || definitive(&terminal).is_some() {
                return NoOp; // already terminal; nothing to submit
            }
            if state.submission_reserved() {
                return PublishTerminal {
                    // impossible under serialized callers; never a 2nd native op
                    candidate: SendTerminal::Internal("reset during submission reservation"),
                    continuation: K::Reset,
                };
            }
            state.reset_submitting = true; // otherwise, incl. incomplete finish
            SubmitReset(code)
        }

        // ── poll_ready request. A present shared terminal is surfaced first.
        PollReady { poll, terminal } => {
            if state.submission_reserved() {
                return PublishTerminal {
                    candidate: SendTerminal::Internal("poll_ready during submission reservation"),
                    continuation: K::PollReady,
                };
            }
            // Method-terminal step: an authoritative winner already in the shared
            // slot is returned before ordinary event handling. A provisional
            // cancellation marker is deliberately NOT surfaced here (MF-2).
            if let Some(w) = definitive(&terminal) {
                state.send_inprogress = false;
                return ReturnError(w);
            }
            if state.finish_started {
                // h3 misuse; no native call, non-sticky (reproduced each call).
                return ReturnError(SendTerminal::Internal("poll_ready after finish"));
            }
            // A retained, unobserved provisional cancellation (MF-2) is pending.
            let provisional = provisional_pending(&terminal);
            match poll {
                // Channel close is a closure point: finalize the provisional to an
                // authoritative abort; otherwise it is an adapter-internal fault.
                SendPoll::Closed if provisional => PublishTerminal {
                    candidate: aborted(),
                    continuation: K::PollReady,
                },
                SendPoll::Closed => PublishTerminal {
                    candidate: SendTerminal::Internal("send channel closed without a terminal"),
                    continuation: K::PollReady,
                },
                SendPoll::Event(SendEvent::Complete { cancelled: false }) => {
                    if !state.send_inprogress {
                        return PublishTerminal {
                            candidate: SendTerminal::Internal(
                                "SendComplete without an outstanding send",
                            ),
                            continuation: K::PollReady,
                        };
                    }
                    state.send_inprogress = false;
                    ReturnReady
                }
                SendPoll::Event(SendEvent::Complete { cancelled: true }) => {
                    if !state.send_inprogress {
                        return PublishTerminal {
                            candidate: SendTerminal::Internal(
                                "cancelled SendComplete without an outstanding send",
                            ),
                            continuation: K::PollReady,
                        };
                    }
                    state.send_inprogress = false;
                    // No authoritative winner (handled above): retain an UNOBSERVED
                    // provisional cancellation marker in the shared slot (MF-2). It is
                    // never returned to the caller — the `PublishTerminal` winner is
                    // fed back as a provisional `TerminalPublished`, which re-polls.
                    PublishTerminal {
                        candidate: SendTerminal::ProvisionalAbort,
                        continuation: K::PollReady,
                    }
                }
                // A lone terminal wake normally means a specific cause was published
                // (surfaced above). With a provisional pending and no specific cause,
                // treat it as the closure point and finalize to an authoritative abort.
                SendPoll::Event(SendEvent::TerminalWake) if provisional => PublishTerminal {
                    candidate: aborted(),
                    continuation: K::PollReady,
                },
                SendPoll::Event(SendEvent::TerminalWake) => PublishTerminal {
                    candidate: SendTerminal::Internal("terminal wake without a terminal"),
                    continuation: K::PollReady,
                },
                SendPoll::Event(SendEvent::FinishComplete { .. }) => PublishTerminal {
                    candidate: SendTerminal::Internal("finish event observed on poll_ready"),
                    continuation: K::PollReady,
                },
                SendPoll::Pending => {
                    if state.send_inprogress {
                        Pending
                    } else if provisional {
                        // Retained provisional cancellation, no closure event yet:
                        // wait (the repoll registered the waker) — never ReturnReady.
                        Pending
                    } else {
                        ReturnReady
                    }
                }
            }
        }

        // ── poll_finish request. Absorbing finish and a just-completed graceful
        //    finish both precede the method-terminal step and event handling.
        PollFinish { poll, terminal } => {
            if state.finish_complete {
                return ReturnFinished; // absorbing
            }
            if state.submission_reserved() {
                return PublishTerminal {
                    candidate: SendTerminal::Internal("poll_finish during submission reservation"),
                    continuation: K::PollFinish,
                };
            }
            // Documented successful-finish exception: a graceful finish that
            // actually completed wins even over a present terminal.
            if let SendPoll::Event(SendEvent::FinishComplete { graceful }) = poll {
                if !state.finish_started {
                    return PublishTerminal {
                        candidate: SendTerminal::Internal(
                            "finish complete without a started finish",
                        ),
                        continuation: K::PollFinish,
                    };
                }
                if graceful {
                    state.finish_complete = true;
                    return ReturnFinished;
                }
                // graceful == false while finishing is a closure point: an
                // authoritative winner wins, else finalize to an abort.
                return PublishTerminal {
                    candidate: definitive(&terminal).unwrap_or_else(aborted),
                    continuation: K::PollFinish,
                };
            }
            // Method-terminal step: after the finish exceptions, an authoritative
            // winner is surfaced before any Pending/SubmitGraceful. A provisional
            // cancellation marker is deliberately NOT surfaced here (MF-2).
            if let Some(w) = definitive(&terminal) {
                state.send_inprogress = false;
                return ReturnError(w);
            }
            let provisional = provisional_pending(&terminal);
            match poll {
                // Channel close is a closure point for a retained provisional.
                SendPoll::Closed if provisional => PublishTerminal {
                    candidate: aborted(),
                    continuation: K::PollFinish,
                },
                SendPoll::Closed => PublishTerminal {
                    candidate: SendTerminal::Internal("send channel closed without a terminal"),
                    continuation: K::PollFinish,
                },
                SendPoll::Event(SendEvent::Complete { cancelled: false })
                    if !state.finish_started =>
                {
                    if !state.send_inprogress {
                        return PublishTerminal {
                            candidate: SendTerminal::Internal(
                                "SendComplete without an outstanding send",
                            ),
                            continuation: K::PollFinish,
                        };
                    }
                    state.send_inprogress = false;
                    state.finish_submitting = true;
                    SubmitGraceful
                }
                SendPoll::Event(SendEvent::Complete { cancelled: true })
                    if !state.finish_started =>
                {
                    if !state.send_inprogress {
                        return PublishTerminal {
                            candidate: SendTerminal::Internal(
                                "cancelled SendComplete without an outstanding send",
                            ),
                            continuation: K::PollFinish,
                        };
                    }
                    state.send_inprogress = false;
                    // Retain an UNOBSERVED provisional cancellation marker (MF-2); the
                    // provisional `TerminalPublished` re-polls rather than returning it.
                    PublishTerminal {
                        candidate: SendTerminal::ProvisionalAbort,
                        continuation: K::PollFinish,
                    }
                }
                // A lone terminal wake with a provisional pending is the closure point.
                SendPoll::Event(SendEvent::TerminalWake) if provisional => PublishTerminal {
                    candidate: aborted(),
                    continuation: K::PollFinish,
                },
                SendPoll::Event(SendEvent::TerminalWake) => PublishTerminal {
                    candidate: SendTerminal::Internal("terminal wake without a terminal"),
                    continuation: K::PollFinish,
                },
                SendPoll::Pending if state.finish_started => Pending,
                SendPoll::Pending if state.send_inprogress => Pending, // wait for in-flight send
                // Retained provisional cancellation, no closure event yet: wait for
                // the closure point rather than submitting a graceful finish on a
                // cancelled stream (the repoll already registered the waker).
                SendPoll::Pending if provisional => Pending,
                SendPoll::Pending => {
                    state.finish_submitting = true;
                    SubmitGraceful // idle, not finishing
                }
                // Any remaining event/state pair (e.g. a data completion while
                // finishing) is adapter-internal, never a panic.
                SendPoll::Event(_) => PublishTerminal {
                    candidate: SendTerminal::Internal("impossible poll_finish event/state"),
                    continuation: K::PollFinish,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_application_code_table() {
        // (name, input, expected)
        let cases: &[(&str, u64, u64)] = &[
            ("small value unchanged", 42, 42),
            ("zero unchanged", 0, 0),
            ("max varint unchanged", (1u64 << 62) - 1, (1u64 << 62) - 1),
            ("one over max clamps", 1u64 << 62, (1u64 << 62) - 1),
            ("u64::MAX clamps", u64::MAX, (1u64 << 62) - 1),
        ];
        for (name, input, expected) in cases {
            assert_eq!(clamp_application_code(*input), *expected, "case: {name}");
        }
    }

    #[test]
    fn max_quic_varint_value() {
        assert_eq!(MAX_QUIC_VARINT, (1u64 << 62) - 1);
    }

    /// Error-mapping matrix (Phases 3–7): every adapter terminal → h3 result row
    /// from the design's "Required mapping" conversion table, asserted through the
    /// authoritative `convert_conn` / `convert_recv` / `convert_send` helpers.
    /// These are the ONLY sites that mint h3 error values, so proving the table
    /// here proves the whole propagation surface's terminal-to-h3 contract.
    #[test]
    fn convert_conn_matrix() {
        // PeerApplication(code) → ApplicationClose { code }
        match convert_conn(ConnectionTerminal::PeerApplication(0x42)) {
            ConnectionErrorIncoming::ApplicationClose { error_code } => {
                assert_eq!(error_code, 0x42)
            }
            other => panic!("PeerApplication → {other:?}"),
        }
        // Timeout → Timeout
        assert!(matches!(
            convert_conn(ConnectionTerminal::Timeout),
            ConnectionErrorIncoming::Timeout
        ));
        // Transport { status, error_code } → Undefined(MsQuicTransportError)
        match convert_conn(ConnectionTerminal::Transport {
            status: Status::new(StatusCode::QUIC_STATUS_TLS_ERROR),
            error_code: 7,
        }) {
            ConnectionErrorIncoming::Undefined(e) => {
                let t = e
                    .downcast_ref::<MsQuicTransportError>()
                    .expect("Transport → MsQuicTransportError");
                // Both the wire transport error code AND the accompanying QUIC
                // status must be preserved verbatim through the conversion.
                assert_eq!(t.error_code, 7, "transport error code preserved");
                assert_eq!(
                    t.status
                        .try_as_status_code()
                        .expect("known transport status"),
                    StatusCode::QUIC_STATUS_TLS_ERROR,
                    "transport status preserved verbatim"
                );
            }
            other => panic!("Transport → {other:?}"),
        }
        // LocalClose → Undefined(LocalConnectionClose)
        match convert_conn(ConnectionTerminal::LocalClose) {
            ConnectionErrorIncoming::Undefined(e) => {
                assert!(e.downcast_ref::<LocalConnectionClose>().is_some());
            }
            other => panic!("LocalClose → {other:?}"),
        }
        // Internal(msg) → InternalError(msg)
        match convert_conn(ConnectionTerminal::Internal("boom")) {
            ConnectionErrorIncoming::InternalError(m) => assert_eq!(m, "boom"),
            other => panic!("Internal → {other:?}"),
        }
    }

    #[test]
    fn convert_recv_matrix() {
        // Fin → Ok(None)
        assert!(matches!(convert_recv(ReceiveTerminal::Fin), Ok(None)));
        // Reset(code) → StreamTerminated { code }
        match convert_recv(ReceiveTerminal::Reset(0x55)) {
            Err(StreamErrorIncoming::StreamTerminated { error_code }) => {
                assert_eq!(error_code, 0x55)
            }
            other => panic!("Reset → {other:?}"),
        }
        // Connection(reason) → ConnectionErrorIncoming using the conn conversion
        match convert_recv(ReceiveTerminal::Connection(ConnectionTerminal::Timeout)) {
            Err(StreamErrorIncoming::ConnectionErrorIncoming { connection_error }) => {
                assert!(matches!(connection_error, ConnectionErrorIncoming::Timeout));
            }
            other => panic!("Connection → {other:?}"),
        }
        // Internal(msg) → ConnectionErrorIncoming(InternalError)
        match convert_recv(ReceiveTerminal::Internal("recv-boom")) {
            Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(m),
            }) => assert_eq!(m, "recv-boom"),
            other => panic!("Internal → {other:?}"),
        }
    }

    #[test]
    fn convert_send_matrix() {
        // Stopped(code) → StreamTerminated { code }
        match convert_send(SendTerminal::Stopped(0x66)) {
            StreamErrorIncoming::StreamTerminated { error_code } => assert_eq!(error_code, 0x66),
            other => panic!("Stopped → {other:?}"),
        }
        // Connection(reason) → ConnectionErrorIncoming using the conn conversion
        match convert_send(SendTerminal::Connection(
            ConnectionTerminal::PeerApplication(9),
        )) {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::ApplicationClose { error_code },
            } => assert_eq!(error_code, 9),
            other => panic!("Connection → {other:?}"),
        }
        // LocalReset(code) → Unknown(LocalStreamReset)
        match convert_send(SendTerminal::LocalReset(0x77)) {
            StreamErrorIncoming::Unknown(e) => {
                let r = e
                    .downcast_ref::<LocalStreamReset>()
                    .expect("LocalReset → LocalStreamReset");
                assert_eq!(r.code, 0x77);
            }
            other => panic!("LocalReset → {other:?}"),
        }
        // Failed(status) → Unknown(status), preserving the exact QUIC status.
        match convert_send(SendTerminal::Failed(Status::new(
            StatusCode::QUIC_STATUS_ABORTED,
        ))) {
            StreamErrorIncoming::Unknown(e) => {
                let s = e.downcast_ref::<Status>().expect("Failed → Status");
                assert_eq!(
                    s.try_as_status_code().expect("known failed status"),
                    StatusCode::QUIC_STATUS_ABORTED,
                    "failed send status preserved verbatim"
                );
            }
            other => panic!("Failed → {other:?}"),
        }
        // ProvisionalAbort → Unknown(aborted status) (defensive fallback); the
        // synthesized status is the authoritative QUIC_STATUS_ABORTED.
        match convert_send(SendTerminal::ProvisionalAbort) {
            StreamErrorIncoming::Unknown(e) => {
                let s = e
                    .downcast_ref::<Status>()
                    .expect("ProvisionalAbort → Status");
                assert_eq!(
                    s.try_as_status_code().expect("known provisional status"),
                    StatusCode::QUIC_STATUS_ABORTED,
                    "provisional abort maps to the aborted status"
                );
            }
            other => panic!("ProvisionalAbort → {other:?}"),
        }
        // Internal(msg) → ConnectionErrorIncoming(InternalError)
        match convert_send(SendTerminal::Internal("send-boom")) {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(m),
            } => assert_eq!(m, "send-boom"),
            other => panic!("Internal → {other:?}"),
        }
    }
}

/// Table-driven tests for the pure send-side reducer (Phase 7).
///
/// Every case drives [`transition`] directly against a [`SendState`], so the
/// full send state machine is exercised with NO native handle. The reducer is
/// pure (mutates only `state`, emits one [`SendCommand`]); the frontend executor
/// loops and the shared-slot first-writer refinement (`publish_send`) live in
/// `stream.rs`/`terminal.rs` and are covered by the `send_seam` tests
/// (`send_seam.rs`).
#[cfg(test)]
mod reducer_tests {
    use super::*;

    // `SendCommand`/`SendTerminal` embed `Status`, which is not `PartialEq`, so
    // these helpers pattern-match rather than compare by value.

    /// A `Connection(LocalClose)` terminal, a convenient concrete winner.
    fn conn_terminal() -> SendTerminal {
        SendTerminal::Connection(ConnectionTerminal::LocalClose)
    }

    /// Drive the (state, input) -> command reducer once.
    fn step(state: &mut SendState, input: SendInput) -> SendCommand {
        transition(state, input)
    }

    // ── MF-1: chronological finish-vs-terminal ordering via the single source ──

    #[test]
    fn mf1_finish_first_completes_cleanly_then_absorbs_terminal() {
        // A finish was submitted; its completion is dequeued BEFORE any terminal
        // wake. Success is absorbing: the later terminal cannot overwrite it.
        let mut st = SendState {
            finish_started: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Event(SendEvent::FinishComplete { graceful: true }),
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::ReturnFinished));
        assert!(st.finish_complete, "graceful completion marks finish done");

        // A terminal wake dequeued afterwards is absorbed: still a clean finish.
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Event(SendEvent::TerminalWake),
                terminal: Some(conn_terminal()),
            },
        );
        assert!(
            matches!(cmd, SendCommand::ReturnFinished),
            "a later terminal must not overwrite a successful finish"
        );
    }

    #[test]
    fn mf1_terminal_first_wins_over_later_finish() {
        // The terminal wake is dequeued FIRST (its cause already published). The
        // finish never completes: the terminal is surfaced as the error.
        let mut st = SendState {
            finish_started: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Event(SendEvent::TerminalWake),
                terminal: Some(SendTerminal::Stopped(7)),
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Stopped(7)) => {}
            other => panic!("expected ReturnError(Stopped(7)), got {other:?}"),
        }
        assert!(
            !st.finish_complete,
            "a terminal-first order must not mark finish complete"
        );
    }

    #[test]
    fn finish_event_not_overwritten_by_later_terminal() {
        // Absorbing success precedes the method-terminal step even when a winner
        // is present in the slot.
        let mut st = SendState {
            finish_complete: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Pending,
                terminal: Some(conn_terminal()),
            },
        );
        assert!(matches!(cmd, SendCommand::ReturnFinished));
    }

    // ── SF-2: poll_ready-after-finish is a non-sticky internal error ──

    #[test]
    fn poll_ready_after_finish_is_nonsticky_internal() {
        // The reducer arm returns an Internal error without mutating state (the
        // frontend guard additionally avoids consuming the channel; see the
        // liveness test in `send_seam.rs`).
        let mut st = SendState {
            finish_started: true,
            ..SendState::new()
        };
        let before = st;
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Pending,
                terminal: None,
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Internal(m)) => {
                assert_eq!(m, "poll_ready after finish")
            }
            other => panic!("expected ReturnError(Internal), got {other:?}"),
        }
        assert_eq!(st, before, "poll_ready-after-finish must not mutate state");
    }

    // ── reset: infallible, at most one clamped native RESET_STREAM ──

    #[test]
    fn reset_emits_single_submit_reset_then_publishes_local_reset() {
        let mut st = SendState::new();
        let code = (1u64 << 62) - 1;
        let cmd = step(
            &mut st,
            SendInput::Reset {
                code,
                terminal: None,
            },
        );
        match cmd {
            SendCommand::SubmitReset(c) => assert_eq!(c, code),
            other => panic!("expected SubmitReset, got {other:?}"),
        }
        assert!(st.reset_submitting, "reset reserves its submission");

        let cmd = step(
            &mut st,
            SendInput::ResetSubmitted {
                code,
                result: Ok(()),
                terminal: None,
            },
        );
        match cmd {
            SendCommand::PublishTerminal {
                candidate: SendTerminal::LocalReset(c),
                continuation: TerminalContinuation::Reset,
            } => assert_eq!(c, code),
            other => panic!("expected PublishTerminal(LocalReset), got {other:?}"),
        }
        assert!(!st.reset_submitting, "the reservation is cleared");

        // The resolved winner is fed back: reset is infallible, so NoOp.
        let cmd = step(
            &mut st,
            SendInput::TerminalPublished {
                winner: SendTerminal::LocalReset(code),
                continuation: TerminalContinuation::Reset,
            },
        );
        assert!(matches!(cmd, SendCommand::NoOp));
    }

    #[test]
    fn reset_after_terminal_is_noop() {
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::Reset {
                code: 5,
                terminal: Some(SendTerminal::Stopped(9)),
            },
        );
        assert!(matches!(cmd, SendCommand::NoOp));
        assert!(!st.reset_submitting, "no native reset after a terminal");
    }

    #[test]
    fn reset_after_finish_complete_is_noop() {
        let mut st = SendState {
            finish_complete: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::Reset {
                code: 5,
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::NoOp));
    }

    #[test]
    fn local_reset_loses_to_published_peer_terminal() {
        // Local reset raced a peer STOP_SENDING that landed before ResetSubmitted:
        // the callback-published terminal wins, no second native op.
        let mut st = SendState::new();
        let _ = step(
            &mut st,
            SendInput::Reset {
                code: 3,
                terminal: None,
            },
        );
        let cmd = step(
            &mut st,
            SendInput::ResetSubmitted {
                code: 3,
                result: Ok(()),
                terminal: Some(SendTerminal::Stopped(9)),
            },
        );
        match cmd {
            SendCommand::PublishTerminal {
                candidate: SendTerminal::Stopped(9),
                continuation: TerminalContinuation::Reset,
            } => {}
            other => panic!("expected PublishTerminal(Stopped(9)), got {other:?}"),
        }
        assert!(!st.reset_submitting, "reservation cleared on the race");
    }

    #[test]
    fn local_reset_loses_to_published_connection_terminal() {
        // Same race against a connection shutdown.
        let mut st = SendState::new();
        let _ = step(
            &mut st,
            SendInput::Reset {
                code: 3,
                terminal: None,
            },
        );
        let cmd = step(
            &mut st,
            SendInput::ResetSubmitted {
                code: 3,
                result: Ok(()),
                terminal: Some(conn_terminal()),
            },
        );
        assert!(matches!(
            cmd,
            SendCommand::PublishTerminal {
                candidate: SendTerminal::Connection(_),
                continuation: TerminalContinuation::Reset,
            }
        ));
    }

    // ── idempotent finish: exactly one graceful shutdown ──

    #[test]
    fn idempotent_finish_submits_graceful_once() {
        let mut st = SendState::new();
        // First poll_finish on an idle stream submits the graceful shutdown.
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Pending,
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::SubmitGraceful));
        assert!(st.finish_submitting);

        // The submit succeeds: reservation clears, finish is started, re-poll.
        let cmd = step(
            &mut st,
            SendInput::GracefulSubmitted {
                result: Ok(()),
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::RepollFinish));
        assert!(st.finish_started && !st.finish_submitting);

        // A subsequent poll_finish while awaiting completion does NOT re-submit.
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Pending,
                terminal: None,
            },
        );
        assert!(
            matches!(cmd, SendCommand::Pending),
            "no second graceful shutdown while finishing"
        );
    }

    // ── *Submitted without a reservation is internal, never a panic ──

    #[test]
    fn submitted_without_reservation_is_internal() {
        for input in [
            SendInput::SendSubmitted {
                result: Ok(()),
                terminal: None,
            },
            SendInput::GracefulSubmitted {
                result: Ok(()),
                terminal: None,
            },
            SendInput::ResetSubmitted {
                code: 1,
                result: Ok(()),
                terminal: None,
            },
        ] {
            let mut st = SendState::new();
            let cmd = step(&mut st, input);
            assert!(
                matches!(
                    cmd,
                    SendCommand::PublishTerminal {
                        candidate: SendTerminal::Internal(_),
                        ..
                    }
                ),
                "a *Submitted without a reservation must publish Internal"
            );
        }
    }

    // ── send_data request precedence ──

    #[test]
    fn send_requested_terminal_wins() {
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::SendRequested {
                payload: SendPayload::NonEmpty { len: 4 },
                terminal: Some(SendTerminal::Stopped(1)),
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Stopped(1)) => {}
            other => panic!("expected ReturnError(Stopped(1)), got {other:?}"),
        }
        assert!(!st.send_submitting, "no submission behind a terminal");
    }

    #[test]
    fn send_requested_while_busy_is_misuse() {
        let mut st = SendState {
            send_inprogress: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::SendRequested {
                payload: SendPayload::NonEmpty { len: 4 },
                terminal: None,
            },
        );
        assert!(matches!(
            cmd,
            SendCommand::ReturnImmediateError(SendOperationError::Misuse(_))
        ));
    }

    #[test]
    fn send_empty_is_noop_and_oversized_is_immediate_error() {
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::SendRequested {
                payload: SendPayload::Empty,
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::ReturnSent));
        assert!(!st.send_submitting, "empty send reserves nothing");

        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::SendRequested {
                payload: SendPayload::Oversized {
                    len: 999,
                    ceiling: 4096,
                },
                terminal: None,
            },
        );
        assert!(matches!(
            cmd,
            SendCommand::ReturnImmediateError(SendOperationError::OversizedSend {
                len: 999,
                ceiling: 4096
            })
        ));
        assert_eq!(
            st,
            SendState::new(),
            "oversized rejection leaves state clean"
        );
    }

    #[test]
    fn send_requested_nonempty_reserves_and_submits() {
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::SendRequested {
                payload: SendPayload::NonEmpty { len: 8 },
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::SubmitSend));
        assert!(st.send_submitting);

        let cmd = step(
            &mut st,
            SendInput::SendSubmitted {
                result: Ok(()),
                terminal: None,
            },
        );
        assert!(matches!(cmd, SendCommand::ReturnSent));
        assert!(st.send_inprogress && !st.send_submitting);
    }

    // ── SC-011 / MF-2: send-cancellation retained-provisional / closure point ──

    #[test]
    fn sc011_cancelled_complete_no_cause_retains_unobserved_provisional() {
        // A cancelled SendComplete with NO published cause retains the DISTINCT
        // `ProvisionalAbort` marker in the shared slot (MF-2). It is provisional
        // (refinable) and, crucially, is fed back as a provisional `TerminalPublished`
        // that RE-POLLS rather than returning — the caller never observes it.
        let mut st = SendState {
            send_inprogress: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Event(SendEvent::Complete { cancelled: true }),
                terminal: None,
            },
        );
        match cmd {
            SendCommand::PublishTerminal {
                candidate,
                continuation: TerminalContinuation::PollReady,
            } => {
                assert!(candidate.is_provisional(), "retains a provisional marker");
                assert!(
                    matches!(candidate, SendTerminal::ProvisionalAbort),
                    "the marker is the distinct ProvisionalAbort, not a real Failed"
                );
            }
            other => panic!("expected PublishTerminal(ProvisionalAbort), got {other:?}"),
        }
        assert!(!st.send_inprogress);

        // The provisional winner fed back must NOT be returned to the caller; it
        // re-polls to reach the closure point / drain a paired terminal.
        let cmd = step(
            &mut st,
            SendInput::TerminalPublished {
                winner: SendTerminal::ProvisionalAbort,
                continuation: TerminalContinuation::PollReady,
            },
        );
        assert!(
            matches!(cmd, SendCommand::RepollReady),
            "a provisional winner re-polls (unobserved), never ReturnError"
        );
    }

    #[test]
    fn mf2_provisional_refines_to_specific_before_observation() {
        // Cancellation-FIRST order: after the provisional is retained, a paired peer
        // cause published into the slot is surfaced on the re-poll — the caller's
        // FIRST observation is the refined specific cause, never the abort.
        let mut st = SendState {
            send_inprogress: false, // cleared by the prior cancelled completion
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Pending,
                terminal: Some(SendTerminal::Stopped(9)), // refined in the slot
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Stopped(9)) => {}
            other => panic!("expected refined ReturnError(Stopped(9)), got {other:?}"),
        }
    }

    #[test]
    fn mf2_provisional_channel_close_is_closure_point_to_authoritative_abort() {
        // Channel close with a retained provisional and no richer cause is the
        // closure point: it finalizes to an AUTHORITATIVE (non-provisional) abort.
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Closed,
                terminal: Some(SendTerminal::ProvisionalAbort),
            },
        );
        match cmd {
            SendCommand::PublishTerminal {
                candidate,
                continuation: TerminalContinuation::PollReady,
            } => {
                assert!(
                    !candidate.is_provisional(),
                    "closure abort is authoritative"
                );
                assert!(matches!(candidate, SendTerminal::Failed(_)));
            }
            other => panic!("expected authoritative Failed at closure, got {other:?}"),
        }
    }

    #[test]
    fn mf2_provisional_pending_channel_pending_waits_not_ready() {
        // While a provisional is retained and no closure event has arrived, an idle
        // Pending poll must WAIT (never spuriously report ReturnReady/ready).
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Pending,
                terminal: Some(SendTerminal::ProvisionalAbort),
            },
        );
        assert!(
            matches!(cmd, SendCommand::Pending),
            "a retained provisional must wait, not report ready"
        );
    }

    #[test]
    fn mf2_provisional_finish_abort_is_closure_point() {
        // poll_finish observing a graceful==false finish while a provisional is
        // retained finalizes to an authoritative abort (closure point).
        let mut st = SendState {
            finish_started: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Event(SendEvent::FinishComplete { graceful: false }),
                terminal: Some(SendTerminal::ProvisionalAbort),
            },
        );
        match cmd {
            SendCommand::PublishTerminal { candidate, .. } => {
                assert!(!candidate.is_provisional());
                assert!(matches!(candidate, SendTerminal::Failed(_)));
            }
            other => panic!("expected authoritative Failed at finish closure, got {other:?}"),
        }
    }

    #[test]
    fn sc011_cancelled_complete_with_peer_stop_surfaces_peer_cause() {
        // Peer STOP_SENDING already published: the method-terminal step surfaces
        // the specific peer cause ahead of the cancelled-completion synthesis.
        let mut st = SendState {
            send_inprogress: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Event(SendEvent::Complete { cancelled: true }),
                terminal: Some(SendTerminal::Stopped(5)),
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Stopped(5)) => {}
            other => panic!("expected ReturnError(Stopped(5)), got {other:?}"),
        }
    }

    #[test]
    fn sc011_cancelled_complete_with_connection_surfaces_connection_cause() {
        // Connection shutdown published with an outstanding send: the connection
        // cause wins over the provisional abort.
        let mut st = SendState {
            send_inprogress: true,
            ..SendState::new()
        };
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Event(SendEvent::Complete { cancelled: true }),
                terminal: Some(conn_terminal()),
            },
        );
        assert!(matches!(
            cmd,
            SendCommand::ReturnError(SendTerminal::Connection(_))
        ));
    }

    // ── STOP_SENDING observable with no send in flight ──

    #[test]
    fn stop_sending_without_send_observable_at_poll_ready_and_finish() {
        // No outstanding send; a sticky peer terminal is surfaced before any
        // early Ok on both poll_ready and poll_finish.
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Pending,
                terminal: Some(SendTerminal::Stopped(3)),
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Stopped(3)) => {}
            other => panic!("poll_ready: expected Stopped(3), got {other:?}"),
        }

        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::PollFinish {
                poll: SendPoll::Pending,
                terminal: Some(SendTerminal::Stopped(3)),
            },
        );
        match cmd {
            SendCommand::ReturnError(SendTerminal::Stopped(3)) => {}
            other => panic!("poll_finish: expected Stopped(3), got {other:?}"),
        }
    }

    #[test]
    fn terminal_wake_without_terminal_is_internal() {
        // A wake with no published cause is an adapter fault, never a panic.
        let mut st = SendState::new();
        let cmd = step(
            &mut st,
            SendInput::PollReady {
                poll: SendPoll::Event(SendEvent::TerminalWake),
                terminal: None,
            },
        );
        assert!(matches!(
            cmd,
            SendCommand::PublishTerminal {
                candidate: SendTerminal::Internal(_),
                ..
            }
        ));
    }

    #[test]
    fn provisional_abort_is_refinable_but_specific_is_not() {
        // is_provisional gates the shared-slot refinement (publish_send in terminal.rs).
        // MF-2 (Item 2): ONLY the distinct synthesized `ProvisionalAbort` marker is
        // provisional/refinable; a real native `Failed` — even `QUIC_STATUS_ABORTED`
        // — is authoritative and NEVER refinable.
        assert!(SendTerminal::ProvisionalAbort.is_provisional());
        assert!(
            !SendTerminal::Failed(aborted_status()).is_provisional(),
            "a REAL native Failed(ABORTED) is authoritative, not provisional"
        );
        assert!(!SendTerminal::Stopped(1).is_provisional());
        assert!(!conn_terminal().is_provisional());
        assert!(!SendTerminal::LocalReset(1).is_provisional());
        assert!(!SendTerminal::Internal("x").is_provisional());
    }
}
