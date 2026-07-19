use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use h3::error::Code;
use h3::proto::frame::Frame;
use h3::quic::{ConnectionErrorIncoming, OpenStreams, RecvStream, SendStream, StreamErrorIncoming};
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
