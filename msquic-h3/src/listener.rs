use std::{net::SocketAddr, sync::Arc};

use futures::ready;
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use msquic::{BufferRef, ListenerEvent, ListenerRef, Status};

use crate::registration::{RundownGuard, RundownState};

pub struct Listener {
    inner: msquic::Listener,
    conn: ListenerCtxReceiver,
    // Dropped last: `inner`'s ListenerClose releases the native rundown ref
    // before this guard decrements and wakes `wait_idle` waiters.
    _guard: RundownGuard,
}

struct ListenerCtxSender {
    conn: Option<mpsc::UnboundedSender<Option<crate::Connection>>>,
    shutdown: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}
struct ListenerCtxReceiver {
    conn: mpsc::UnboundedReceiver<Option<crate::Connection>>,
    /// mutex used to make shutdown immutable
    shutdown: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

fn listener_ctx_channel() -> (ListenerCtxSender, ListenerCtxReceiver) {
    let (tx, rx) = mpsc::unbounded();
    let (sh_tx, sh_rx) = oneshot::channel();
    (
        ListenerCtxSender {
            conn: Some(tx),
            shutdown: std::sync::Mutex::new(Some(sh_tx)),
        },
        ListenerCtxReceiver {
            conn: rx,
            shutdown: std::sync::Mutex::new(Some(sh_rx)),
        },
    )
}

/// The single-outcome decision for a listener `NewConnection` event, factored
/// out of the FFI callback so every branch is unit-testable without a forgeable
/// `ConnectionRef`.
///
/// The listener double-close hazard lives entirely in *which* of these three
/// outcomes runs; keeping the choice in a pure function lets tests assert it
/// directly while [`listener_callback`] performs the matching ownership action.
#[derive(Debug)]
enum NewConnDecision {
    /// `set_configuration` failed on the borrowed `ConnectionRef` *before* any
    /// ownership was taken. The callback returns this `Status` so native
    /// `listener.c` performs the single close; the reserved rundown guard is
    /// released (never `from_raw`, never a Rust-side close).
    ConfigFailedClose(Status),
    /// Configuration succeeded and the owned connection was handed to the accept
    /// frontend. The callback returns `Ok`.
    DeliveredOwned,
    /// Configuration succeeded and ownership was taken, but frontend delivery
    /// failed (receiver lost). The owned `Connection` is dropped (a single
    /// `ConnectionClose`) and the callback returns `Ok` — never close-then-reject
    /// the same handle (which would trip native `listener.c`'s assert).
    DeliveryFailedDropOwned,
}

/// Pure decision logic for the listener `NewConnection` event.
///
/// `config` is the result of `set_configuration` run through the *borrowed*
/// `ConnectionRef`. Only when it succeeds is `deliver` invoked; that closure
/// takes native ownership (`from_raw`), attaches, and attempts frontend
/// delivery, returning `true` on delivery and `false` if the receiver was lost
/// (dropping the now-owned connection). On a config failure `deliver` is never
/// called, so no ownership is ever taken — and any rundown guard the closure
/// captured is released when the un-invoked closure is dropped here.
fn decide_new_connection(
    config: Result<(), Status>,
    deliver: impl FnOnce() -> bool,
) -> NewConnDecision {
    match config {
        Err(e) => NewConnDecision::ConfigFailedClose(e),
        Ok(()) => {
            if deliver() {
                NewConnDecision::DeliveredOwned
            } else {
                NewConnDecision::DeliveryFailedDropOwned
            }
        }
    }
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(skip(ctx, config, state), level = "trace", ret, err)
)]
fn listener_callback(
    ctx: &ListenerCtxSender,
    ev: ListenerEvent,
    config: &Arc<msquic::Configuration>,
    state: &Arc<RundownState>,
) -> Result<(), Status> {
    match ev {
        ListenerEvent::NewConnection {
            info: _,
            connection,
        } => {
            // `connection` is the borrowed `ConnectionRef`; the native handle
            // already holds a registration rundown ref, so reserve our count now.
            let guard = RundownGuard::new(state.clone());
            // Validate/configure through the BORROWED ref, before taking
            // ownership. `ConnectionRef` derefs to `Connection` and its `Drop`
            // only nulls the handle — it never closes it.
            let config = connection.set_configuration(config);
            // The `deliver` closure runs (taking ownership) only if `config`
            // succeeded. It captures `guard` by move: on the config-failure path
            // it is dropped un-invoked, releasing the reserved rundown count with
            // no `from_raw` and no Rust-side close — native `listener.c` performs
            // the single close on our returned `Err`.
            let decision = decide_new_connection(config, move || {
                // `set_configuration` succeeded: only NOW take ownership.
                let inner = unsafe { msquic::Connection::from_raw(connection.as_raw()) };
                let conn = crate::Connection::attach(inner, guard);
                match ctx.conn.as_ref() {
                    Some(tx) => match tx.unbounded_send(Some(conn)) {
                        Ok(()) => true,
                        // Delivery failed after ownership: the `SendError`
                        // carries the owned `Connection`, dropped here for a
                        // single `ConnectionClose`.
                        Err(send_err) => {
                            drop(send_err.into_inner());
                            false
                        }
                    },
                    // No sender (listener frontend gone): drop the owned
                    // `Connection` for a single close.
                    None => {
                        drop(conn);
                        false
                    }
                }
            });
            match decision {
                NewConnDecision::ConfigFailedClose(e) => return Err(e),
                NewConnDecision::DeliveredOwned | NewConnDecision::DeliveryFailedDropOwned => {}
            }
        }
        ListenerEvent::StopComplete { .. } => {
            // none means end of connections
            if let Some(tx) = ctx.conn.as_ref() {
                let _ = tx.unbounded_send(None);
            }
            let tx = crate::lock_recover(&ctx.shutdown).take();
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
        }
    }
    Ok(())
}

impl Listener {
    pub fn new(
        reg: &crate::Registration,
        config: Arc<msquic::Configuration>,
        alpn: &[BufferRef],
        local_addr: Option<SocketAddr>,
    ) -> Result<Self, Status> {
        let (tx, rx) = listener_ctx_channel();
        let state = reg.state().clone();
        let handler =
            move |_: ListenerRef, ev: ListenerEvent| listener_callback(&tx, ev, &config, &state);
        // Reserve the listener's guard BEFORE opening the native handle:
        // `ListenerOpen` acquires the registration rundown before it returns, so
        // reserving first ensures an in-flight construction is always counted.
        // If `open` fails, this local guard drops and releases the reservation.
        let guard = RundownGuard::new(reg.state().clone());
        let inner = msquic::Listener::open(reg.raw(), handler)?;
        let addr = local_addr.map(msquic::Addr::from);
        // Build the struct before `start` so a start failure drops fields in
        // declaration order (ListenerClose before the guard decrement).
        let listener = Self {
            inner,
            conn: rx,
            _guard: guard,
        };
        listener.inner.start(alpn, addr.as_ref())?;
        Ok(listener)
    }

    /// Get the inner listener ref.
    pub fn get_ref(&self) -> &msquic::Listener {
        &self.inner
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    pub fn poll_accept(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<crate::Connection>, Status>> {
        let s = ready!(self.conn.conn.poll_next_unpin(cx)).unwrap_or(None);
        std::task::Poll::Ready(Ok(s))
    }

    pub async fn accept(&mut self) -> Result<Option<crate::Connection>, Status> {
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// shutdown is made immutable to enable it to be called from another thread.
    pub async fn shutdown(&self) {
        let opt_rx = {
            let mut lk = crate::lock_recover(&self.conn.shutdown);
            lk.take()
        };
        if let Some(rx) = opt_rx {
            self.inner.stop();
            // On cancellation (sender dropped) treat as already shut down and
            // return rather than panicking. Calling `shutdown` twice is a no-op
            // because the receiver was taken above.
            let _ = rx.await;
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        sync::Arc,
    };

    use msquic::{BufferRef, CredentialConfig, CredentialFlags, RegistrationConfig, Settings};
    use tracing::info;

    use crate::Listener;

    #[test]
    fn basic_server_test() {
        crate::test::util::try_setup_tracing();
        info!("Test start");
        let cred = crate::test::util::get_test_cred();

        let reg = crate::Registration::new(&RegistrationConfig::default()).unwrap();
        let alpn = [BufferRef::from("h3")];
        let settings = Settings::new()
            .set_ServerResumptionLevel(msquic::ServerResumptionLevel::ResumeAndZerortt)
            .set_PeerBidiStreamCount(1)
            .set_IdleTimeoutMs(1000);

        let config = reg.open_configuration(&alpn, Some(&settings)).unwrap();

        let cred_config = CredentialConfig::new()
            .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION)
            .set_credential(cred);
        config.load_credential(&cred_config).unwrap();

        let config = Arc::new(config);

        let mut l = Listener::new(
            &reg,
            config,
            &alpn,
            Some(SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                4568,
            )),
        )
        .unwrap();

        let (sht_tx, mut sht_rx) = tokio::sync::oneshot::channel::<()>();
        let th = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut i = 0;
                loop {
                    let conn_id = i;
                    info!("server accept conn {}", i);
                    i += 1;
                    let conn = tokio::select! {
                        val = l.accept() => val.unwrap(),
                        _ = &mut sht_rx => {
                            info!("server accepted interrupted.");
                            None // stop accept and break.
                        }
                    };
                    if conn.is_none() {
                        info!("server accepted conn end");
                        break;
                    }
                    // let rth = rt.handle().clone();
                    // use another task to handle conn
                    rt.spawn(async move {
                        let conn = conn.unwrap();
                        info!("server accepted conn id={}", conn_id);
                        info!("server conn connect");
                        let mut h3_conn: h3::server::Connection<crate::Connection, bytes::Bytes> =
                            h3::server::Connection::new(conn).await.unwrap();
                        loop {
                            match h3_conn.accept().await {
                                Ok(Some(resolver)) => {
                                    tokio::spawn(async move {
                                        let (req, mut stream) =
                                            match resolver.resolve_request().await {
                                                Ok(req) => req,
                                                Err(e) => {
                                                    info!("fail resolve request {e:#?}");
                                                    return;
                                                }
                                            };
                                        info!("new request: {:#?}", req);
                                        drop(req);
                                        let resp =
                                            http::Response::builder().status(200).body(()).unwrap();

                                        // send headers
                                        match stream.send_response(resp).await {
                                            Ok(_) => {
                                                tracing::info!(
                                                    "successfully respond to connection"
                                                );
                                            }
                                            Err(err) => {
                                                tracing::error!(
                                                "unable to send response to connection peer: {:?}",
                                                err
                                            );
                                            }
                                        }
                                        // send body
                                        let body = bytes::Bytes::from_static(b"mydata");
                                        match stream.send_data(body).await {
                                            Ok(_) => tracing::info!("send body ok"),
                                            Err(e) => tracing::error!("send body err: {e}"),
                                        }
                                        // tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                        // close stream. it sends stuff without check ready.
                                        match stream.finish().await {
                                            Ok(_) => {
                                                tracing::info!("close stream ok")
                                            }
                                            Err(e) => tracing::error!("close stream err: {e}"),
                                        }

                                        // TODO: stream drop can happen to quickly.
                                        // tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                    });
                                }

                                // indicating no more streams to be received
                                Ok(None) => {
                                    break;
                                }

                                Err(err) => {
                                    tracing::error!("error on accept {}", err);
                                    break;
                                }
                            }
                        }
                    });
                }
                info!("server listener stop");
                l.shutdown().await;
                info!("server listner stop finish");
            });
            info!("tokio server end.");
        });

        // std::thread::sleep(Duration::from_secs(100));
        // send request
        let uri = http::Uri::from_static("https://127.0.0.1:4568");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(crate::test::send_get_request(uri));
        //std::thread::sleep(Duration::from_secs(1));
        let _ = sht_tx.send(());
        th.join().unwrap();
    }

    /// A live listener holds a rundown guard, so `wait_idle` must stay pending
    /// until the listener is dropped (no client involved). Guards against the
    /// listener-rundown hang and the reserve-before-open race.
    #[test]
    fn listener_keeps_wait_idle_pending() {
        use std::time::Duration;

        crate::test::util::try_setup_tracing();
        let cred = crate::test::util::get_test_cred();

        let reg = crate::Registration::new(&RegistrationConfig::default()).unwrap();
        let alpn = [BufferRef::from("h3")];
        let settings = Settings::new().set_IdleTimeoutMs(1000);
        let config = reg.open_configuration(&alpn, Some(&settings)).unwrap();
        let cred_config = CredentialConfig::new()
            .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION)
            .set_credential(cred);
        config.load_credential(&cred_config).unwrap();
        let config = Arc::new(config);

        let l = Listener::new(
            &reg,
            config,
            &alpn,
            Some(SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                0, // ephemeral port; no client connects here
            )),
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            // While the listener is alive, wait_idle must not resolve.
            let pending = tokio::time::timeout(Duration::from_millis(100), reg.wait_idle()).await;
            assert!(
                pending.is_err(),
                "wait_idle resolved while a listener was still alive"
            );

            // Dropping the listener runs ListenerClose then decrements the guard.
            drop(l);

            tokio::time::timeout(Duration::from_secs(2), reg.wait_idle())
                .await
                .expect("wait_idle should resolve after the listener is dropped");
        });
    }
}

/// Phase 5 listener connection failure-path unit tests. The real `NewConnection`
/// callback is FFI-driven and a `ConnectionRef` cannot be forged hermetically,
/// so the single-outcome decision is factored into [`decide_new_connection`] and
/// asserted directly for all three branches: pre-ownership `set_configuration`
/// failure (single close / `Err`), post-ownership delivery success (owned +
/// `Ok`), and post-ownership delivery failure (owned dropped + `Ok`). The real
/// success path is covered end-to-end by `basic_server_test`.
#[cfg(test)]
mod new_conn_decision {
    use std::{cell::Cell, sync::Arc};

    use msquic::{Status, StatusCode};

    use super::{NewConnDecision, decide_new_connection};
    use crate::registration::{RundownGuard, RundownState};

    /// Pre-ownership `set_configuration` failure: the deliver closure (which is
    /// the only thing that takes native ownership) never runs, the decision is
    /// `ConfigFailedClose` carrying the status the callback returns as `Err` (so
    /// native `listener.c` performs the single close), and the reserved rundown
    /// guard captured by the un-invoked closure is released.
    #[test]
    fn config_failure_closes_once_without_taking_ownership() {
        let state = Arc::new(RundownState::default());
        let base = Arc::strong_count(&state);
        let guard = RundownGuard::new(state.clone());
        // Reserving a guard clones the shared `RundownState` Arc.
        assert_eq!(Arc::strong_count(&state), base + 1);

        let delivered = Cell::new(false);
        let status = Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR);
        let expected = status.try_as_status_code().ok();
        let decision = decide_new_connection(Err(status), || {
            // Would take ownership + deliver in production; must not run here.
            delivered.set(true);
            let _consumes_guard = guard;
            true
        });

        match decision {
            NewConnDecision::ConfigFailedClose(e) => {
                assert_eq!(e.try_as_status_code().ok(), expected);
            }
            other => panic!("expected ConfigFailedClose, got {other:?}"),
        }
        assert!(
            !delivered.get(),
            "no ownership must be taken when set_configuration fails (single native close)"
        );
        // The un-invoked closure dropped the guard it captured, releasing the
        // reserved rundown count back to the pre-reservation baseline.
        assert_eq!(Arc::strong_count(&state), base);
    }

    /// Post-ownership delivery success: configuration passed and the owned
    /// connection reached the frontend → `DeliveredOwned` (callback returns `Ok`).
    #[test]
    fn delivery_success_reports_delivered_owned() {
        let decision = decide_new_connection(Ok(()), || true);
        assert!(matches!(decision, NewConnDecision::DeliveredOwned));
    }

    /// Post-ownership delivery failure (receiver lost): configuration passed and
    /// ownership was taken, but delivery failed → `DeliveryFailedDropOwned`. The
    /// callback drops the owned connection (single close) and returns `Ok` —
    /// never close-then-reject.
    #[test]
    fn delivery_failure_reports_drop_owned() {
        let decision = decide_new_connection(Ok(()), || false);
        assert!(matches!(decision, NewConnDecision::DeliveryFailedDropOwned));
    }
}
