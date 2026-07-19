//! Callback-safety primitives (SF-E / FR-007): the panic-containment guard and
//! the injectable native force-close seam shared by the connection, stream, and
//! listener FFI callback surfaces.
//!
//! [`guard_callback`] runs each adapter callback body under `catch_unwind` so a
//! contained panic never unwinds across the FFI boundary; on a caught panic it
//! runs the class-appropriate `recover` closure through the [`ShutdownSeam`]
//! seam, poisons the ctx, and returns the caller-precomputed disposition.

use msquic::{ConnectionRef, ConnectionShutdownFlags, Status, StreamRef, StreamShutdownFlags};

/// The callback class a contained panic originated in. Retained only as a label
/// for the structured error event (`report_contained_panic`); the
/// class-appropriate recovery behavior lives in the `recover` closure passed at
/// each registration site, not in a `match` on this label inside the guard.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CbClass {
    Connection,
    Stream,
    Listener,
}

/// The native force-close a panic-recovery action requests through the
/// injectable [`ShutdownSeam`]. The exact flags are carried here (chosen by the
/// class `recover`) so the `callback_safety` unit-test double can assert them
/// without a real native handle: streams abort (`StreamShutdownFlags::ABORT`);
/// connections have no `ABORT` flag, so the abort is conveyed by the application
/// `code` alongside `ConnectionShutdownFlags::NONE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ForceShutdown {
    Connection(ConnectionShutdownFlags),
    Stream(StreamShutdownFlags),
}

/// Injectable seam for the native force-close performed during callback panic
/// recovery. In production the seam is the callback-time BORROWED handle ref
/// (`ConnectionRef`/`StreamRef`), whose `force_shutdown` delegates to the real
/// FFI `shutdown`; the `callback_safety` unit tests inject a no-op double that
/// records the request without any native handle. The listener class does not
/// force-close, so it is wired with [`NoShutdown`].
pub(crate) trait ShutdownSeam {
    /// Perform the requested native force-close with the given application code.
    fn force_shutdown(&self, request: ForceShutdown, code: u64);
}

/// Production connection seam: the borrowed `ConnectionRef` (auto-derefs to
/// `Connection`). Acts only on a `Connection` request; a mismatched request
/// never occurs (the connection `recover` only ever asks for a connection
/// shutdown) and is a safe no-op.
impl ShutdownSeam for ConnectionRef {
    fn force_shutdown(&self, request: ForceShutdown, code: u64) {
        if let ForceShutdown::Connection(flags) = request {
            self.shutdown(flags, code);
        }
    }
}

/// Production stream seam: the borrowed `StreamRef` (auto-derefs to `Stream`).
/// Acts only on a `Stream` request; the fallible native `shutdown` result is
/// discarded (the handle is being abandoned regardless).
impl ShutdownSeam for StreamRef {
    fn force_shutdown(&self, request: ForceShutdown, code: u64) {
        if let ForceShutdown::Stream(flags) = request {
            let _ = self.shutdown(flags, code);
        }
    }
}

/// A no-op shutdown seam. Used by the listener registration site (the listener
/// recover is ownership-aware and never force-closes) and available to tests.
pub(crate) struct NoShutdown;

impl ShutdownSeam for NoShutdown {
    fn force_shutdown(&self, _request: ForceShutdown, _code: u64) {}
}

/// A callback context carrying a panic-poison flag. Once a contained panic has
/// poisoned the ctx, [`guard_callback`] short-circuits every later event with
/// the caller-precomputed event-aware disposition instead of dispatching the
/// body again.
pub(crate) trait PoisonFlag {
    fn is_poisoned(&self) -> bool;
}

/// Emit a structured error event for a contained callback panic. A no-op unless
/// the `tracing` feature is enabled (no logging dependency is required
/// otherwise).
#[cfg(feature = "tracing")]
pub(crate) fn report_contained_panic(class: CbClass) {
    tracing::error!(?class, "FFI callback panic contained (SF-E)");
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn report_contained_panic(_class: CbClass) {}

/// Run an adapter callback `body` under panic containment (SF-E / FR-007).
///
/// BORROW DISCIPLINE (must compile): `body` holds the ONLY `&mut ctx` borrow
/// while it runs inside `catch_unwind(AssertUnwindSafe(..))`. That borrow ENDS
/// when `catch_unwind` returns, so `recover` — called only on the `Err` path,
/// AFTER `catch_unwind` returns — legally re-borrows `&mut ctx`. There is no
/// overlapping borrow, so neither the (non-`Clone`/`Copy`) handle nor the
/// ctx-owned senders are captured by value; both the `&mut ctx` and the
/// callback-time borrowed handle ref are passed as PARAMETERS.
///
/// - Already poisoned by a prior contained panic → return the caller-precomputed
///   event-aware `poisoned_result` (teardown/terminal events → `Ok(())`;
///   ownership-bearing events → `Err(INTERNAL_ERROR)` so msquic rejects them).
/// - A real `Err` from the body (e.g. the Phase 2 receive `PENDING`) flows
///   through unchanged; `recover` is NOT run.
/// - A caught panic → run `recover(ctx, handle)` (force-close + wake waiters +
///   poison) and return its status.
pub(crate) fn guard_callback<C: PoisonFlag>(
    ctx: &mut C,
    handle: &dyn ShutdownSeam,
    poisoned_result: Result<(), Status>,
    body: impl FnOnce(&mut C) -> Result<(), Status>,
    recover: impl FnOnce(&mut C, &dyn ShutdownSeam) -> Result<(), Status>,
) -> Result<(), Status> {
    if ctx.is_poisoned() {
        return poisoned_result;
    }
    // `body(&mut *ctx)` is a reborrow that lives only for the `catch_unwind`
    // call; after it returns the borrow is released and `ctx` is free again.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&mut *ctx))) {
        Ok(r) => r,
        Err(_) => recover(ctx, handle),
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
        let (mut ctx, rx) = conn_ctx_channel(crate::H3Config::default());
        // Frontend gone: the `Connected` one-shot receiver is dropped.
        drop(rx);
        let ev = ConnectionEvent::Connected {
            session_resumed: false,
            negotiated_alpn: &[],
        };
        // The fallible send returns Err internally; the callback must not panic
        // and must return Ok across the FFI boundary.
        assert!(connection_callback(&mut ctx, ev, &std::cell::Cell::new(false)).is_ok());
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

    // ── FFI callback panic containment (SF-E / FR-007) ──────────────────────
    //
    // These are HERMETIC: no `Registration`, no port, no native endpoint. The
    // native force-close is routed through the injectable [`ShutdownSeam`], so a
    // recording no-op double stands in for the real `StreamRef`/`ConnectionRef`.

    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::error::{SendEvent, SendTerminal};
    use crate::msquic::{ConnectionShutdownFlags, Status, StatusCode, StreamShutdownFlags};
    use crate::{
        ForceShutdown, H3_INTERNAL_ERROR, PoisonFlag, ReceiveEvent, ShutdownSeam, conn_poison_disp,
        connection_recover, guard_callback, load_winner, new_conn_terminal_slot,
        stream_ctx_channel_pre_id, stream_poison_disp, stream_recover,
    };

    fn is_internal_err(r: &Result<(), Status>) -> bool {
        matches!(r, Err(s) if s.0 == Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR).0)
    }

    fn is_status(r: &Result<(), Status>, code: StatusCode) -> bool {
        matches!(r, Err(s) if s.0 == Status::new(code).0)
    }

    /// A no-op [`ShutdownSeam`] double recording every force-close request, so a
    /// unit test can assert the exact flags/code without a native handle.
    #[derive(Default)]
    struct RecordingSeam {
        calls: RefCell<Vec<(ForceShutdown, u64)>>,
    }

    impl ShutdownSeam for RecordingSeam {
        fn force_shutdown(&self, request: ForceShutdown, code: u64) {
            self.calls.borrow_mut().push((request, code));
        }
    }

    /// A minimal poison-carrying ctx for direct `guard_callback` unit tests.
    struct TestCtx {
        poisoned: bool,
    }
    impl PoisonFlag for TestCtx {
        fn is_poisoned(&self) -> bool {
            self.poisoned
        }
    }

    #[test]
    fn guard_callback_passes_through_results_without_recovering() {
        // Ok flows through; recover never runs, ctx never poisoned.
        let mut ctx = TestCtx { poisoned: false };
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            Ok(()),
            |_c| Ok(()),
            |_c, _s| panic!("recover must NOT run for a real body result"),
        );
        assert!(got.is_ok());
        assert!(!ctx.poisoned);

        // A real Err (INTERNAL) flows through unchanged.
        let mut ctx = TestCtx { poisoned: false };
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            Ok(()),
            |_c| Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c, _s| panic!("recover must NOT run for a real body result"),
        );
        assert!(is_internal_err(&got));
        assert!(!ctx.poisoned);

        // The Phase 2 receive PENDING is a real Err and flows through unchanged.
        let mut ctx = TestCtx { poisoned: false };
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            Ok(()),
            |_c| Err(Status::new(StatusCode::QUIC_STATUS_PENDING)),
            |_c, _s| panic!("recover must NOT run for a real body result"),
        );
        assert!(is_status(&got, StatusCode::QUIC_STATUS_PENDING));
        assert!(!ctx.poisoned);
    }

    #[test]
    fn guard_callback_runs_recover_once_on_panic() {
        let mut ctx = TestCtx { poisoned: false };
        let ran = RefCell::new(0u32);
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            Ok(()),
            |_c| panic!("boom in body"),
            |c: &mut TestCtx, _s| {
                *ran.borrow_mut() += 1;
                c.poisoned = true;
                Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR))
            },
        );
        assert!(is_internal_err(&got), "recover status is surfaced");
        assert_eq!(*ran.borrow(), 1, "recover runs exactly once");
        assert!(ctx.poisoned);
    }

    #[test]
    fn guard_callback_poisoned_returns_disposition_without_body() {
        // Poisoned + teardown disposition Ok → Ok, body not run.
        let mut ctx = TestCtx { poisoned: true };
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            Ok(()),
            |_c| panic!("body must NOT run when poisoned"),
            |_c, _s| panic!("recover must NOT run when poisoned"),
        );
        assert!(got.is_ok());

        // Poisoned + ownership-bearing disposition Err → Err, body not run.
        let mut ctx = TestCtx { poisoned: true };
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| panic!("body must NOT run when poisoned"),
            |_c, _s| panic!("recover must NOT run when poisoned"),
        );
        assert!(is_internal_err(&got));
    }

    #[test]
    fn connection_callback_panic_is_contained() {
        let (mut ctx, mut crx) = conn_ctx_channel(crate::H3Config::default());
        let seam = RecordingSeam::default();
        // A panicking body is contained: process survives (test completes), the
        // connection is force-closed via the seam, and Err(INTERNAL_ERROR) returns.
        let got = guard_callback(
            &mut ctx,
            &seam,
            conn_poison_disp(&ConnectionEvent::Connected {
                session_resumed: false,
                negotiated_alpn: &[],
            }),
            |_c| panic!("boom in connection callback"),
            |c, seam| connection_recover(c, seam, &Cell::new(false)),
        );
        assert!(is_internal_err(&got));
        assert_eq!(
            *seam.calls.borrow(),
            vec![(
                ForceShutdown::Connection(ConnectionShutdownFlags::NONE),
                H3_INTERNAL_ERROR
            )],
            "connection force-close uses NONE + H3_INTERNAL_ERROR"
        );
        assert!(ctx.poisoned, "ctx is poisoned after a contained panic");
        // Waiter-wake regression: the connect one-shot is resolved with a failure
        // rather than left hanging.
        let mut woken = crx.connected.take().expect("connect waiter present");
        match woken.try_recv() {
            Ok(Some(Err(_))) => {}
            other => panic!("connect waiter should observe a failure, got {other:?}"),
        }
        // Both accept frontends were dropped (a parked acceptor observes closure).
        assert!(ctx.bidi.is_none() && ctx.uni.is_none());
    }

    #[test]
    fn connection_poison_short_circuit_is_event_aware() {
        // After a contained panic poisons the ctx, a teardown event short-circuits
        // to Ok (no-op) and a non-teardown (ownership-bearing) event to Err.
        let (mut ctx, _crx) = conn_ctx_channel(crate::H3Config::default());
        let _ = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            conn_poison_disp(&ConnectionEvent::Connected {
                session_resumed: false,
                negotiated_alpn: &[],
            }),
            |_c| panic!("boom"),
            |c, seam| connection_recover(c, seam, &Cell::new(false)),
        );
        assert!(ctx.poisoned);

        // Teardown (ShutdownComplete) → Ok, body not run.
        let teardown = ConnectionEvent::ShutdownComplete {
            handshake_completed: false,
            peer_acknowledged_shutdown: false,
            app_close_in_progress: false,
        };
        assert!(conn_poison_disp(&teardown).is_ok());
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            conn_poison_disp(&teardown),
            |_c| panic!("body must NOT run when poisoned"),
            |c, seam| connection_recover(c, seam, &Cell::new(false)),
        );
        assert!(got.is_ok());

        // Non-teardown event → Err (msquic rejects). `PeerStreamStarted` cannot be
        // forged hermetically (it carries a native `StreamRef`); its Err mapping is
        // the same `_ => Err` catch-all exercised here via `Connected`.
        let ownership_like = ConnectionEvent::Connected {
            session_resumed: false,
            negotiated_alpn: &[],
        };
        assert!(is_internal_err(&conn_poison_disp(&ownership_like)));
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            conn_poison_disp(&ownership_like),
            |_c| panic!("body must NOT run when poisoned"),
            |c, seam| connection_recover(c, seam, &Cell::new(false)),
        );
        assert!(is_internal_err(&got));
    }

    #[test]
    fn connection_teardown_body_panic_is_contained_and_wakes_waiters() {
        // A panic while handling a TEARDOWN event (ShutdownComplete) is still
        // contained: recover force-closes, wakes the connect waiter, and poisons
        // the ctx — so a task parked on the connect one-shot cannot hang. A
        // subsequent teardown then short-circuits to Ok WITHOUT re-running the
        // body, so Phase 2 terminal state is never double-completed.
        let (mut ctx, mut crx) = conn_ctx_channel(crate::H3Config::default());
        let seam = RecordingSeam::default();
        let teardown = ConnectionEvent::ShutdownComplete {
            handshake_completed: false,
            peer_acknowledged_shutdown: false,
            app_close_in_progress: false,
        };
        // First event: ctx not yet poisoned, so the body runs and panics.
        let got = guard_callback(
            &mut ctx,
            &seam,
            conn_poison_disp(&teardown),
            |_c| panic!("boom while handling ShutdownComplete"),
            |c, seam| connection_recover(c, seam, &Cell::new(false)),
        );
        assert!(is_internal_err(&got));
        assert_eq!(
            *seam.calls.borrow(),
            vec![(
                ForceShutdown::Connection(ConnectionShutdownFlags::NONE),
                H3_INTERNAL_ERROR
            )],
            "teardown-body panic still force-closes once"
        );
        assert!(
            ctx.poisoned,
            "ctx is poisoned after a contained teardown panic"
        );
        // Waiter-wake: the connect one-shot is resolved with a failure, not hung.
        let mut woken = crx.connected.take().expect("connect waiter present");
        match woken.try_recv() {
            Ok(Some(Err(_))) => {}
            other => panic!("connect waiter should observe a failure, got {other:?}"),
        }
        // A subsequent teardown short-circuits to Ok WITHOUT running the body or
        // recover again — no double force-close, no double-completion.
        let got = guard_callback(
            &mut ctx,
            &seam,
            conn_poison_disp(&teardown),
            |_c| panic!("body must NOT run on teardown after poison"),
            |c, seam| connection_recover(c, seam, &Cell::new(false)),
        );
        assert!(got.is_ok());
        assert_eq!(
            seam.calls.borrow().len(),
            1,
            "no second force-close after poison short-circuit"
        );
    }

    /// RAII stand-in for the owned peer `Stream` that `Stream::from_raw` hands to
    /// `H3Stream::attach` on `PeerStreamStarted`. Its `Drop` increments a shared
    /// counter, mirroring how the real owned `Stream`'s `Drop` performs the single
    /// native `StreamClose`. Constructing one models `from_raw` taking ownership;
    /// the unwind dropping it models the close — so the close is directly
    /// OBSERVABLE (counter), not merely inferred from the recover return value.
    struct OwnedStreamStandIn(Arc<AtomicUsize>);
    impl Drop for OwnedStreamStandIn {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn connection_peer_stream_panic_before_from_raw_rejects() {
        // F-C: a `PeerStreamStarted` panic BEFORE `Stream::from_raw` never took
        // native stream ownership, so no owned stand-in exists (close == 0) and
        // `connection_recover` returns Err — msquic performs the single stream
        // reject. The connection is still force-closed and its waiters woken.
        let (mut ctx, mut crx) = conn_ctx_channel(crate::H3Config::default());
        let seam = RecordingSeam::default();
        // Per-invocation peer-stream ownership token, never shared with the ctx.
        let stream_owned = Cell::new(false);
        let closes = Arc::new(AtomicUsize::new(0));
        let got = guard_callback(
            &mut ctx,
            &seam,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| {
                // Ownership never transferred: from_raw never runs, no stand-in.
                let _keep_alive = &closes;
                panic!("boom before PeerStreamStarted from_raw");
            },
            |c, seam| connection_recover(c, seam, &stream_owned),
        );
        // No stream ownership → Err: msquic performs the sole stream reject.
        assert!(
            is_internal_err(&got),
            "pre-ownership PeerStreamStarted panic → Err (single msquic reject)"
        );
        assert_eq!(
            closes.load(Ordering::Relaxed),
            0,
            "no owned stream stand-in exists, so zero close occurs"
        );
        // The connection itself is still force-closed exactly once.
        assert_eq!(
            *seam.calls.borrow(),
            vec![(
                ForceShutdown::Connection(ConnectionShutdownFlags::NONE),
                H3_INTERNAL_ERROR
            )],
            "connection is force-closed once regardless of stream ownership"
        );
        assert!(ctx.poisoned);
        // Connection waiters woken so a parked connect/accept task cannot hang.
        let mut woken = crx.connected.take().expect("connect waiter present");
        match woken.try_recv() {
            Ok(Some(Err(_))) => {}
            other => panic!("connect waiter should observe a failure, got {other:?}"),
        }
        assert!(ctx.bidi.is_none() && ctx.uni.is_none());
    }

    #[test]
    fn connection_peer_stream_panic_after_from_raw_does_not_reject() {
        // F-C: a `PeerStreamStarted` panic AFTER `Stream::from_raw` (e.g. inside
        // `H3Stream::attach`) already took native stream ownership — the owned
        // stream's Drop performs the single `StreamClose` (close == 1). msquic must
        // NOT re-close it, so `connection_recover` returns Ok (no double-close),
        // while still force-closing the CONNECTION and waking its waiters.
        let (mut ctx, mut crx) = conn_ctx_channel(crate::H3Config::default());
        let seam = RecordingSeam::default();
        let stream_owned = Cell::new(false);
        let closes = Arc::new(AtomicUsize::new(0));
        let got = guard_callback(
            &mut ctx,
            &seam,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| {
                // Emulate from_raw taking stream ownership: construct the owned
                // stand-in and mark the token the instant it transfers. The named
                // binding lives until the panic, so the unwind's Drop performs the
                // single close (counter += 1).
                let _owned_stream = OwnedStreamStandIn(closes.clone());
                stream_owned.set(true);
                panic!("boom after PeerStreamStarted from_raw");
            },
            |c, seam| connection_recover(c, seam, &stream_owned),
        );
        // Stream ownership already taken + dropped → Ok: msquic must NOT re-close.
        assert!(
            got.is_ok(),
            "post-ownership PeerStreamStarted panic → Ok (owned stream Drop closed it once)"
        );
        assert_eq!(
            closes.load(Ordering::Relaxed),
            1,
            "the owned stream stand-in's Drop performs exactly one close"
        );
        // The connection is still force-closed exactly once.
        assert_eq!(
            *seam.calls.borrow(),
            vec![(
                ForceShutdown::Connection(ConnectionShutdownFlags::NONE),
                H3_INTERNAL_ERROR
            )],
            "connection is force-closed once even when stream ownership was taken"
        );
        assert!(ctx.poisoned);
        // Connection waiters woken so a parked connect/accept task cannot hang.
        let mut woken = crx.connected.take().expect("connect waiter present");
        match woken.try_recv() {
            Ok(Some(Err(_))) => {}
            other => panic!("connect waiter should observe a failure, got {other:?}"),
        }
        assert!(ctx.bidi.is_none() && ctx.uni.is_none());
    }

    #[test]
    fn stream_callback_panic_is_contained() {
        let (mut ctx, mut recv) =
            stream_ctx_channel_pre_id(new_conn_terminal_slot(), crate::H3Config::default());
        let seam = RecordingSeam::default();
        let got = guard_callback(
            &mut ctx,
            &seam,
            stream_poison_disp(&StreamEvent::PeerSendShutdown),
            |_c| panic!("boom in stream callback"),
            stream_recover,
        );
        assert!(is_internal_err(&got));
        assert_eq!(
            *seam.calls.borrow(),
            vec![(
                ForceShutdown::Stream(StreamShutdownFlags::ABORT),
                H3_INTERNAL_ERROR
            )],
            "stream force-close uses ABORT + H3_INTERNAL_ERROR"
        );
        assert!(ctx.poisoned);
        // Send half woken with an internal terminal (waiter-wake regression).
        match load_winner(&recv.send_terminal) {
            Some(SendTerminal::Internal(_)) => {}
            other => panic!("send terminal should be Internal, got {other:?}"),
        }
        match recv.send.try_recv() {
            Ok(SendEvent::TerminalWake) => {}
            other => panic!("send half should be woken, got {other:?}"),
        }
        // Receive half woken with a STREAM-LOCAL internal terminal (F-A): a
        // stream-scoped `ReceiveEvent::Internal`, NOT a `Connection` event, so it
        // never freezes the shared connection slot.
        match recv.receive.try_recv() {
            Ok(ReceiveEvent::Internal(_)) => {}
            _ => panic!("receive half should be woken with a stream-local Internal terminal"),
        }
        // Pending open (StartComplete) waiter resolved with a failure, not hung.
        match recv.start.try_recv() {
            Ok(Some(Err(_))) => {}
            other => panic!("open waiter should observe a failure, got {other:?}"),
        }
    }

    #[test]
    fn stream_poison_short_circuit_is_event_aware() {
        let (mut ctx, _recv) =
            stream_ctx_channel_pre_id(new_conn_terminal_slot(), crate::H3Config::default());
        let _ = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            stream_poison_disp(&StreamEvent::PeerSendShutdown),
            |_c| panic!("boom"),
            stream_recover,
        );
        assert!(ctx.poisoned);

        // Teardown (ShutdownComplete) → Ok, body not run.
        let teardown = StreamEvent::ShutdownComplete {
            connection_shutdown: false,
            app_close_in_progress: false,
            connection_shutdown_by_app: false,
            connection_closed_remotely: false,
            connection_error_code: 0,
            connection_close_status: Status::new(StatusCode::QUIC_STATUS_SUCCESS),
        };
        assert!(stream_poison_disp(&teardown).is_ok());
        let got = guard_callback(
            &mut ctx,
            &RecordingSeam::default(),
            stream_poison_disp(&teardown),
            |_c| panic!("body must NOT run when poisoned"),
            stream_recover,
        );
        assert!(got.is_ok());

        // Non-teardown event → Err.
        assert!(is_internal_err(&stream_poison_disp(
            &StreamEvent::PeerSendShutdown
        )));
    }

    #[test]
    fn stream_teardown_body_panic_is_contained_and_wakes_waiters() {
        // A panic while handling a TEARDOWN event (ShutdownComplete) is contained:
        // recover force-aborts, wakes both the send half and the pending open
        // waiter, and poisons the ctx — so no waiter hangs. A subsequent teardown
        // then short-circuits to Ok without re-running the body (no double
        // completion of Phase 2 send/receive terminal state).
        let (mut ctx, mut recv) =
            stream_ctx_channel_pre_id(new_conn_terminal_slot(), crate::H3Config::default());
        let seam = RecordingSeam::default();
        let teardown = StreamEvent::ShutdownComplete {
            connection_shutdown: false,
            app_close_in_progress: false,
            connection_shutdown_by_app: false,
            connection_closed_remotely: false,
            connection_error_code: 0,
            connection_close_status: Status::new(StatusCode::QUIC_STATUS_SUCCESS),
        };
        // First event: ctx not yet poisoned, so the body runs and panics.
        let got = guard_callback(
            &mut ctx,
            &seam,
            stream_poison_disp(&teardown),
            |_c| panic!("boom while handling stream ShutdownComplete"),
            stream_recover,
        );
        assert!(is_internal_err(&got));
        assert_eq!(
            *seam.calls.borrow(),
            vec![(
                ForceShutdown::Stream(StreamShutdownFlags::ABORT),
                H3_INTERNAL_ERROR
            )],
            "teardown-body panic still force-aborts once"
        );
        assert!(ctx.poisoned);
        // Waiter-wake: send half woken with a terminal, open waiter with a failure.
        match recv.send.try_recv() {
            Ok(SendEvent::TerminalWake) => {}
            other => panic!("send half should be woken, got {other:?}"),
        }
        match recv.start.try_recv() {
            Ok(Some(Err(_))) => {}
            other => panic!("open waiter should observe a failure, got {other:?}"),
        }
        // A subsequent teardown short-circuits to Ok WITHOUT re-running the body
        // or recover — no double force-abort, no double-completion.
        let got = guard_callback(
            &mut ctx,
            &seam,
            stream_poison_disp(&teardown),
            |_c| panic!("body must NOT run on teardown after poison"),
            stream_recover,
        );
        assert!(got.is_ok());
        assert_eq!(
            seam.calls.borrow().len(),
            1,
            "no second force-abort after poison short-circuit"
        );
    }
}
