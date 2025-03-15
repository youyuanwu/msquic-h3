use std::{net::SocketAddr, sync::Arc};

use futures_util::ready;
use msquic::{BufferRef, ListenerEvent, ListenerRef, Status};
use tokio::sync::{mpsc, oneshot};

pub struct Listener {
    inner: msquic::Listener,
    conn: ListenerCtxReceiver,
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
    let (tx, rx) = mpsc::unbounded_channel();
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
    tracing::instrument(skip(ctx, config), level = "trace", ret, err)
)]
fn listener_callback(
    ctx: &ListenerCtxSender,
    ev: ListenerEvent,
    config: &Arc<msquic::Configuration>,
) -> Result<(), Status> {
    match ev {
        ListenerEvent::NewConnection {
            info: _,
            connection,
        } => {
            let conn = unsafe { msquic::Connection::from_raw(connection.as_raw()) };
            // TODO: need to set callback.
            // set the config
            conn.set_configuration(config)?;
            let conn = crate::Connection::attach(conn);
            if let Some(tx) = ctx.conn.as_ref() {
                tx.send(Some(conn)).expect("cannot send");
            }
        }
        ListenerEvent::StopComplete { .. } => {
            // none means end of connections
            if let Some(tx) = ctx.conn.as_ref() {
                tx.send(None).expect("cannot send");
            }
            let mut lk = ctx.shutdown.lock().unwrap();
            let tx = lk.take();
            if let Some(tx) = tx {
                tx.send(()).expect("cannot send");
            }
        }
    }
    Ok(())
}

impl Listener {
    pub fn new(
        reg: &msquic::Registration,
        config: Arc<msquic::Configuration>,
        alpn: &[BufferRef],
        local_addr: Option<SocketAddr>,
    ) -> Result<Self, Status> {
        let (tx, rx) = listener_ctx_channel();
        let handler = move |_: ListenerRef, ev: ListenerEvent| listener_callback(&tx, ev, &config);
        let mut inner = msquic::Listener::new();
        inner.open(reg, handler)?;
        let addr = local_addr.map(msquic::Addr::from);
        inner.start(alpn, addr.as_ref())?;
        Ok(Self { inner, conn: rx })
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
        let s = ready!(self.conn.conn.poll_recv(cx)).unwrap_or(None);
        std::task::Poll::Ready(Ok(s))
    }

    pub async fn accept(&mut self) -> Result<Option<crate::Connection>, Status> {
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// shutdown is made immutable to enable it to be called from another thread.
    pub async fn shutdown(&self) {
        let opt_rx = {
            let mut lk = self.conn.shutdown.lock().unwrap();
            lk.take()
        };
        if let Some(rx) = opt_rx {
            self.inner.stop();
            rx.await.expect("cannot receive");
        }
    }
}

#[cfg(target_os = "windows")]
#[cfg(test)]
mod test {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        sync::Arc,
    };

    use msquic::{
        BufferRef, CertificateHash, Configuration, Credential, CredentialConfig, CredentialFlags,
        Registration, RegistrationConfig, Settings,
    };
    use tracing::info;

    use crate::Listener;

    #[test]
    fn basic_server_test() {
        crate::test::util::try_setup_tracing();
        info!("Test start");
        let cert_hash = crate::test::util::get_test_cert_hash();
        info!("Using cert_hash: [{cert_hash}]");

        let reg = Registration::new(&RegistrationConfig::default()).unwrap();
        let alpn = [BufferRef::from("h3")];
        let settings = Settings::new()
            .set_ServerResumptionLevel(msquic::ServerResumptionLevel::ResumeAndZerortt)
            .set_PeerBidiStreamCount(1)
            .set_IdleTimeoutMs(1000);

        let config = Configuration::new(&reg, &alpn, Some(&settings)).unwrap();

        let cred_config = CredentialConfig::new()
            .set_credential_flags(CredentialFlags::NO_CERTIFICATE_VALIDATION)
            .set_credential(Credential::CertificateHash(
                CertificateHash::from_str(&cert_hash).unwrap(),
            ));
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
                                Ok(Some((req, mut stream))) => {
                                    tokio::spawn(async move {
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
                                    match err.get_error_level() {
                                        h3::error::ErrorLevel::ConnectionError => break,
                                        h3::error::ErrorLevel::StreamError => continue,
                                    }
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
        sht_tx.send(()).unwrap();
        th.join().unwrap();
    }
}
