//! Terminal-cause machinery (Group C): the shared, thread-safe slots recording
//! *why* a connection or send half terminated, together with the first-writer /
//! provisional-to-specific refinement rules and the classification helpers that
//! map raw msquic transport/shutdown statuses into a [`ConnectionTerminal`].
//!
//! The connection terminal slot ([`ConnTerminalSlot`]) is written by the
//! connection FFI callback and read by the accept frontends / stream opener; the
//! send terminal slot ([`SendTerminalSlot`]) is written by the stream callback
//! and the send-side reducer. All access goes through [`lock_recover`] so an FFI
//! callback never panics on a poisoned lock.

use std::sync::{Arc, Mutex};

use msquic::{Status, StatusCode};

use crate::error::{ConnectionTerminal, SendTerminal};
use crate::lock_recover;

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
    pub(crate) fn observe(&mut self) -> Option<ConnectionTerminal> {
        self.observed = true;
        self.terminal.clone()
    }

    /// Whether an internal (fail-fast) terminal has been published. Read before
    /// draining queued streams so an internal failure is reported immediately
    /// rather than behind already-queued items.
    pub(crate) fn has_internal(&self) -> bool {
        matches!(self.terminal, Some(ConnectionTerminal::Internal(_)))
    }

    /// Whether a terminal reason has been recorded (regardless of the observed
    /// freeze state). Read by [`commit_conn`] to freeze the slot *only* when a
    /// cause is actually present, so an empty slot is never frozen on a
    /// non-delivery (the commit-on-delivery invariant).
    fn terminal_is_some(&self) -> bool {
        self.terminal.is_some()
    }
}

/// Record a connection terminal reason into the shared slot.
pub(crate) fn record_conn_terminal(slot: &ConnTerminalSlot, candidate: ConnectionTerminal) {
    lock_recover(slot).record(candidate);
}

/// Freeze the shared connection terminal slot and return its winner.
///
/// This is the *stream-side* observation point of the SF-7 / T4 refinement rule
/// (FR-013), retained for [`H3RecvStream`]'s `poll_data` `Connection` arm only,
/// where the shared slot is **always populated** at the delivery point (the
/// stream callback records the connection reason before signalling
/// `ReceiveEvent::Connection`), so its unconditional `observe()` can never
/// freeze an empty slot. Every *other* delivery site (send frontends,
/// stream-open, accept) freezes through [`commit_conn`], which never freezes an
/// empty slot.
pub(crate) fn observe_conn_winner(slot: &ConnTerminalSlot) -> Option<ConnectionTerminal> {
    lock_recover(slot).observe()
}

/// Commit-on-delivery freeze primitive: freeze the shared connection slot **only**
/// when a cause is actually present, and return it (frozen) for delivery to h3.
///
/// This is the single freeze primitive for every h3-facing poll that can surface
/// a connection cause but is *not* guaranteed to hold one at the delivery point:
/// the send frontends (via [`SendStreamReceiveCtx::commit_send_winner`]),
/// [`StreamOpener::poll_open_inner`] and its helpers (SF-C), and the accept
/// frontends (via [`observe_terminal`], Fix 1). Unlike [`observe_conn_winner`] it
/// **never** freezes an empty slot: a non-delivery (an empty slot returning a
/// synthetic internal/unknown error) leaves the slot refinable so a later real
/// cause can still be recorded and delivered (FR-002 / FR-003).
pub(crate) fn commit_conn(slot: &ConnTerminalSlot) -> Option<ConnectionTerminal> {
    let mut g = lock_recover(slot);
    if g.terminal_is_some() {
        g.observe() // sets observed = true, returning the frozen cause
    } else {
        None
    }
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
pub(crate) fn new_send_terminal_slot() -> SendTerminalSlot {
    Arc::new(Mutex::new(None))
}

/// Read the current sticky send winner (poison-safe clone; `None` if unset).
pub(crate) fn load_winner(slot: &SendTerminalSlot) -> Option<SendTerminal> {
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
pub(crate) fn publish_send(slot: &SendTerminalSlot, candidate: SendTerminal) -> SendTerminal {
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
pub(crate) fn classify_transport(status: Status, error_code: u64) -> ConnectionTerminal {
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
/// `docs/receive-and-send.md`, "Receive-side transitions"):
/// - `by_app && closed_remotely` is a peer HTTP/3 application close;
/// - `by_app && !closed_remotely` is a local application close (no peer code);
/// - anything else is a transport close, delegated to [`classify_transport`]
///   (which distinguishes idle/handshake timeouts from generic transport).
pub(crate) fn classify_conn_shutdown(
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

/// Non-freezing read of the connection terminal reason.
///
/// Unlike [`ConnTerminalState::observe`], this clones the recorded reason
/// without marking the slot observed, so the stream-open path can consult the
/// connection terminal without freezing it for the accept frontends.
pub(crate) fn peek_conn_terminal(slot: &ConnTerminalSlot) -> Option<ConnectionTerminal> {
    lock_recover(slot).terminal.clone()
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

    /// A fresh per-invocation peer-stream ownership token for driving
    /// `connection_callback` in these hermetic tests. None of these events is a
    /// `PeerStreamStarted`, so the token is never set (F-C).
    fn no_stream_owned() -> std::cell::Cell<bool> {
        std::cell::Cell::new(false)
    }

    /// Drive one connection event through the callback and return the frozen,
    /// converted terminal the accept frontend would report.
    fn map_event(ev: ConnectionEvent) -> ConnectionErrorIncoming {
        let (mut ctx, crx) = conn_ctx_channel();
        assert!(connection_callback(&mut ctx, ev, &no_stream_owned()).is_ok());
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
        assert!(connection_callback(&mut ctx, ev, &no_stream_owned()).is_ok());
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
        assert!(connection_callback(&mut ctx, ev, &no_stream_owned()).is_ok());
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
                ConnectionEvent::ShutdownInitiatedByPeer { error_code: 9 },
                &no_stream_owned()
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
                },
                &no_stream_owned()
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
                ConnectionEvent::ShutdownInitiatedByPeer { error_code: 11 },
                &no_stream_owned()
            )
            .is_ok()
        );
        assert!(matches!(
            observe_terminal(&crx.terminal),
            ConnectionErrorIncoming::ApplicationClose { error_code: 11 }
        ));
    }

    #[test]
    fn accept_delivering_a_cause_commits_and_locks_identity() {
        // Phase 1 / Fix 1 (SC-003, accept as a committing poll): `observe_terminal`
        // (the accept frontends' delivery) is a COMMITTING poll when it delivers a
        // cause. Record a cause, deliver it (freezing the slot), then a later
        // different cause does not change what a subsequent observer sees.
        let slot = new_conn_terminal_slot();
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
        // The delivery froze the slot: a later refinement is rejected.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(7));
        assert!(matches!(
            observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }

    #[test]
    fn accept_empty_slot_non_delivery_does_not_commit() {
        // Phase 1 / Fix 1 core: an empty slot is a NON-delivery — `observe_terminal`
        // returns `InternalError` WITHOUT freezing (the empty `None` branch leaves
        // the slot refinable), so a later real cause can still be recorded and
        // delivered by a genuine observer.
        let slot = new_conn_terminal_slot();
        match observe_terminal(&slot) {
            ConnectionErrorIncoming::InternalError(msg) => {
                assert!(msg.contains("without a terminal reason"), "msg: {msg}");
            }
            other => panic!("expected InternalError, got {other:?}"),
        }
        // The slot was NOT frozen: a subsequently-recorded genuine cause is delivered.
        record_conn_terminal(&slot, ConnectionTerminal::PeerApplication(9));
        assert!(matches!(
            observe_terminal(&slot),
            ConnectionErrorIncoming::ApplicationClose { error_code: 9 }
        ));
    }
}
