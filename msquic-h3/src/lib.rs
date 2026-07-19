// Native-provenance features `native-find` / `native-src` are mutually exclusive
// (see Cargo.toml). Selecting BOTH is rejected by the upstream `msquic` build
// script (`feature src and find are mutually exclusive`), which runs before this
// crate compiles. Selecting NEITHER is a SUPPORTED type-check-only configuration:
// the crate compiles without linking a native library (this is exactly what the
// default-features CI `cargo check`/`clippy` job exercises), so no crate-level
// guard is imposed. A real build/link that resolves msquic symbols requires
// exactly one provenance. docs.rs builds under `native-src` via
// `[package.metadata.docs.rs]` in Cargo.toml.

use std::sync::{Mutex, MutexGuard, PoisonError};

#[cfg(test)]
pub(crate) use h3::quic::ConnectionErrorIncoming;
use msquic::{Status, StatusCode};

mod buffer;
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
mod connection;
#[cfg(test)]
pub(crate) use connection::{
    ConnCtxReceiver, ConnCtxSender, accept_stream_id, conn_ctx_channel, conn_poison_disp,
    connection_callback, connection_recover, fail_fast_terminal, observe_terminal,
};
pub(crate) use connection::{ConnHandle, validate_stream_id};
pub use connection::{Connection, ConnectionShutdownWaiter};
mod opener;
pub use opener::StreamOpener;
#[cfg(test)]
pub(crate) use opener::{classify_start_outcome, stream_open_conn_error};
mod error;
pub use error::{LocalConnectionClose, LocalStreamReset, MsQuicTransportError, OversizedSend};
mod listener;
pub use listener::Listener;
mod registration;
pub use registration::{Registration, WaitIdle};

mod stream;
#[cfg(test)]
pub(crate) use crate::buffer::SendBuffer;
#[cfg(test)]
pub(crate) use stream::{
    Admit, MAX_RECV_BUFFER, PreIdReceivers, PreIdTail, ReceiveEvent, RecvBudget, RecvExec,
    RecvStreamReceiveCtx, SendExec, SendStreamReceiveCtx, StreamSendCtx, stream_callback,
    stream_ctx_channel, stream_ctx_channel_pre_id, stream_ctx_channel_with_conn,
    stream_poison_disp, stream_recover, submit_owned_send,
};
pub use stream::{H3RecvStream, H3SendStream, H3Stream};
pub(crate) use stream::{OpenExec, OpeningStream, StreamOpenExecutor};

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

/// Phase 6 (Owned `SendBuffer` & command-executor seam) unit tests.
///
/// These prove the send seam mechanism WITHOUT a live connection: an injected
/// [`SendExec`] test double (`CountingExec`) shares the exact allocation/reclaim
/// contract as the production `StreamExecutor`, and the retained `client_context`
/// is replayed through the PRODUCTION [`stream_callback`] — the real reconstruct+
/// drop path — so the exactly-once ownership guarantee is exercised, not mocked.
/// The comprehensive `CountingExec` matrix + loopback ordering test are Phase 8.
#[cfg(test)]
mod send_seam;

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
