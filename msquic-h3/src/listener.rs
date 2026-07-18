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
            let inner = unsafe { msquic::Connection::from_raw(connection.as_raw()) };
            // The native handle already holds a registration rundown ref here;
            // reserve our count immediately.
            let guard = RundownGuard::new(state.clone());
            // set the config
            if let Err(e) = inner.set_configuration(config) {
                // Close the handle first (releases the rundown ref), then the
                // guard decrements.
                drop(crate::ConnHandle::new(inner, guard));
                return Err(e);
            }
            let conn = crate::Connection::attach(inner, guard);
            if let Some(tx) = ctx.conn.as_ref() {
                // Ownership was taken by `Connection::attach`; if delivery fails
                // the returned `SendError` carries the owned `Connection`, which
                // drops here and runs a single `ConnectionClose`. Return `Ok`
                // (never `Err`) so the callback does not also reject an owned
                // handle and trip native `listener.c`'s close-and-reject assert.
                let _ = tx.unbounded_send(Some(conn));
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
