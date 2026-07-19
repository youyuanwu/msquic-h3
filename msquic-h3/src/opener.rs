//! The stream opener (Group E): the public [`StreamOpener`] type and the pure
//! open/start seams it delegates to.
//!
//! [`StreamOpener`] implements `h3::quic::OpenStreams` for a [`Connection`]:
//! it creates a native stream, drives it to a fully-identified [`H3Stream`], and
//! never panics — a connection-caused cancellation of a pending start maps to a
//! connection error via [`stream_open_conn_error`], and a `StartComplete`
//! outcome is classified into a validated local stream ID (or a stream error) by
//! [`classify_start_outcome`]. The open/start downcalls go through the
//! [`OpenExec`] seam so a test can substitute them without a live connection.
//!
//! [`Connection`]: crate::Connection

use std::pin::Pin;
use std::sync::Arc;

use bytes::Buf;
use futures::{channel::oneshot, ready};
use h3::quic::{ConnectionErrorIncoming, OpenStreams, StreamErrorIncoming};
use msquic::Status;

use crate::error::{ConnectionTerminal, clamp_application_code, convert_conn};
use crate::{
    ConnHandle, ConnTerminalSlot, H3SendStream, H3Stream, OpenExec, OpeningStream,
    StreamOpenExecutor, commit_conn, record_conn_terminal, validate_stream_id,
};

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
    pub(crate) fn new(conn: Arc<ConnHandle>) -> Self {
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
pub(crate) fn stream_open_conn_error(slot: &ConnTerminalSlot) -> ConnectionErrorIncoming {
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
pub(crate) fn classify_start_outcome(
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
