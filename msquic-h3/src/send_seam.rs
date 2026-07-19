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
    ConnTerminalSlot, ConnectionErrorIncoming, H3Config, H3SendStream, OversizedSend, SendBuffer,
    SendExec, SendStreamReceiveCtx, StreamSendCtx, new_conn_terminal_slot, record_conn_terminal,
    stream_callback, stream_ctx_channel, stream_ctx_channel_with_config,
    stream_ctx_channel_with_conn,
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

/// Like [`test_send_ctx`] but with an explicit [`H3Config`], so a send-seam
/// test can drive the real `send_data` classification against a **custom**
/// per-send-size ceiling (`config.max_send_bytes()`) instead of the default.
fn test_send_ctx_with_config(config: H3Config) -> (SendStreamReceiveCtx, StreamSendCtx) {
    let id: h3::quic::StreamId = 0u64.try_into().expect("valid StreamId");
    let (ctx, sctx, _rctx) = stream_ctx_channel_with_config(id, new_conn_terminal_slot(), config);
    (sctx, ctx)
}

/// A validated [`H3Config`] with only the per-send-size ceiling overridden.
fn cfg_with_ceiling(ceiling: u64) -> H3Config {
    H3Config::builder()
        .with_max_send_bytes(ceiling)
        .build()
        .expect("valid custom send ceiling")
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

/// SC-001 / SC-004 (send): the real `send_data` path honors a **custom reduced**
/// per-send-size ceiling `C`. The ceiling bounds the full copied frame length
/// (`WriteBuf::remaining()` = the h3 DATA header + payload), so the test sizes
/// its payload so the *framed* length is exactly `C + 1`: it is rejected up
/// front as `OversizedSend` — which now reports the configured ceiling in
/// `max_bytes` and the framed length in `len` — with ZERO copies and ZERO native
/// submissions. This proves `send_data` classifies against
/// `self.sctx.max_send_bytes` (the configured ceiling), not `MAX_ADAPTER_SEND`.
#[test]
fn custom_ceiling_send_data_rejects_oversized_before_allocation() {
    use bytes::Buf;
    const C: u64 = 4096;

    // The h3 DATA framing overhead (`remaining()` - payload) is constant for
    // payload sizes in this magnitude; measure it so the framed length lands on
    // an exact boundary rather than hardcoding the varint header width.
    let framed_len = |payload: Bytes| -> usize {
        let wb: h3::quic::WriteBuf<Bytes> = Frame::Data(payload).into();
        wb.remaining()
    };
    let header = framed_len(Bytes::from(vec![0u8; C as usize - 100])) - (C as usize - 100);
    // Payload whose framed length is exactly C + 1 (one byte over the ceiling).
    let over_payload = Bytes::from(vec![0u8; (C as usize + 1) - header]);
    assert_eq!(framed_len(over_payload.clone()), C as usize + 1);

    let h = CountingHandle::default();
    let exec = Box::new(CountingExec::new(h.clone(), VecDeque::new()));
    let (sctx, _ctx) = test_send_ctx_with_config(cfg_with_ceiling(C));
    let mut s = H3SendStream::with_exec(exec, sctx);

    let err = SendStream::<Bytes>::send_data(&mut s, Frame::Data(over_payload))
        .expect_err("oversized send must be rejected at the custom ceiling");

    match err {
        StreamErrorIncoming::Unknown(e) => {
            let os = e
                .downcast_ref::<OversizedSend>()
                .expect("expected OversizedSend");
            assert_eq!(
                os.len,
                C as usize + 1,
                "reported the rejected (framed) length"
            );
            assert_eq!(
                os.max_bytes, C,
                "OversizedSend must report the CONFIGURED ceiling, not MAX_ADAPTER_SEND"
            );
            assert_ne!(
                os.max_bytes, MAX_ADAPTER_SEND,
                "the custom ceiling must not be the default"
            );
        }
        other => panic!("expected Unknown(OversizedSend), got {other:?}"),
    }
    // Rejected before any copy or native submission, and no send in flight.
    assert_eq!(h.sends.load(Relaxed), 0);
    assert_eq!(h.allocs.load(Relaxed), 0);
    assert!(s.sctx.send.try_recv().is_err());
}

/// Complement to the rejection test: a send whose framed length is EXACTLY the
/// custom ceiling `C` is accepted and submitted (one copy, one native
/// submission), confirming the classifier admits `remaining <= C` rather than
/// rejecting at the boundary.
#[test]
fn custom_ceiling_send_data_accepts_payload_at_the_ceiling() {
    use bytes::Buf;
    const C: u64 = 4096;

    let framed_len = |payload: Bytes| -> usize {
        let wb: h3::quic::WriteBuf<Bytes> = Frame::Data(payload).into();
        wb.remaining()
    };
    let header = framed_len(Bytes::from(vec![0u8; C as usize - 100])) - (C as usize - 100);
    // Payload whose framed length is exactly C (right at the ceiling).
    let at_payload = Bytes::from(vec![0u8; C as usize - header]);
    assert_eq!(framed_len(at_payload.clone()), C as usize);

    let h = CountingHandle::default();
    // A single scripted Ok so the native side "accepts ownership" of the copy.
    let mut script = VecDeque::new();
    script.push_back(Ok(()));
    let exec = Box::new(CountingExec::new(h.clone(), script));
    let (sctx, _ctx) = test_send_ctx_with_config(cfg_with_ceiling(C));
    let mut s = H3SendStream::with_exec(exec, sctx);

    SendStream::<Bytes>::send_data(&mut s, Frame::Data(at_payload))
        .expect("a send exactly at the custom ceiling must be accepted");

    // Exactly one copy and one native submission occurred.
    assert_eq!(h.sends.load(Relaxed), 1);
    assert_eq!(h.allocs.load(Relaxed), 1);
    assert_eq!(h.client_ctx.lock().unwrap().len(), 1);
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
    let err = SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"second")))
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
            std::task::Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code })) => {
                assert_eq!(error_code, 42)
            }
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
    SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"more-data"))).unwrap();
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
    SendStream::<Bytes>::send_data(&mut s, Frame::Data(Bytes::from_static(b"payload"))).unwrap();
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
        Some(crate::error::SendTerminal::Connection(ConnectionTerminal::PeerApplication(9))) => {}
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
        Some(crate::error::SendTerminal::Connection(ConnectionTerminal::PeerApplication(9))) => {}
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
