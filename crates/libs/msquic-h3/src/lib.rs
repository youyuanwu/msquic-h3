use std::{ffi::c_void, pin::Pin, sync::Arc};

use bytes::{Buf, BufMut, BytesMut};
use futures_util::ready;
use h3::quic::{BidiStream, OpenStreams, RecvStream, SendStream};
use msquic::{
    Configuration, ConnectionEvent, ConnectionRef, ConnectionShutdownFlags, ReceiveFlags,
    Registration, SendFlags, Status, StatusCode, StreamEvent, StreamOpenFlags, StreamRef,
    StreamShutdownFlags, StreamStartFlags,
};
use tokio::sync::{mpsc, oneshot};

mod buffer;
pub use buffer::*;
mod listener;
pub use listener::Listener;

/// re-export msquic type
pub mod msquic {
    pub use ::msquic::*;
}

#[derive(Debug)]
pub struct H3Error {
    status: Status,
    error_code: Option<u64>,
}

impl H3Error {
    pub fn new(status: Status, ec: Option<u64>) -> Self {
        Self {
            status,
            error_code: ec,
        }
    }
}

impl h3::quic::Error for H3Error {
    fn is_timeout(&self) -> bool {
        self.status
            .try_as_status_code()
            .unwrap_or(StatusCode::QUIC_STATUS_SUCCESS)
            == StatusCode::QUIC_STATUS_CONNECTION_TIMEOUT
    }

    fn err_code(&self) -> Option<u64> {
        self.error_code
    }
}

impl std::error::Error for H3Error {}

impl std::fmt::Display for H3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug)]
pub struct Connection {
    conn: Arc<msquic::Connection>,
    ctx: ConnCtxReceiver,
    opener: StreamOpener,
}

/// from callback send to fount end.
#[derive(Debug)]
struct ConnCtxSender {
    connected: Option<oneshot::Sender<()>>,
    bidi: Option<mpsc::UnboundedSender<Option<crate::H3Stream>>>,
    uni: Option<mpsc::UnboundedSender<Option<crate::H3Stream>>>,
}

/// front end receive.
#[derive(Debug)]
struct ConnCtxReceiver {
    connected: Option<oneshot::Receiver<()>>,
    bidi: mpsc::UnboundedReceiver<Option<crate::H3Stream>>,
    uni: mpsc::UnboundedReceiver<Option<crate::H3Stream>>,
}

fn conn_ctx_channel() -> (ConnCtxSender, ConnCtxReceiver) {
    let (conn_tx, conn_rx) = oneshot::channel();
    let (bidi_tx, bidi_rx) = mpsc::unbounded_channel();
    let (uni_tx, uni_rx) = mpsc::unbounded_channel();
    (
        ConnCtxSender {
            connected: Some(conn_tx),
            bidi: Some(bidi_tx),
            uni: Some(uni_tx),
        },
        ConnCtxReceiver {
            connected: Some(conn_rx),
            bidi: bidi_rx,
            uni: uni_rx,
        },
    )
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(skip(ctx), level = "trace", ret, err)
)]
fn connection_callback(ctx: &mut ConnCtxSender, ev: msquic::ConnectionEvent) -> Result<(), Status> {
    match ev {
        ConnectionEvent::Connected { .. } => {
            ctx.connected.take().unwrap().send(()).unwrap();
        }
        ConnectionEvent::PeerStreamStarted { stream, flags } => {
            // TODO: need to set callback
            let s = unsafe { msquic::Stream::from_raw(stream.as_raw()) };
            if flags.contains(StreamOpenFlags::UNIDIRECTIONAL) {
                if let Some(uni) = ctx.uni.as_ref() {
                    uni.send(Some(crate::H3Stream::attach(s)))
                        .expect("cannot send");
                }
            } else if let Some(bidi) = ctx.bidi.as_ref() {
                bidi.send(Some(crate::H3Stream::attach(s)))
                    .expect("cannot send");
            }
        }
        ConnectionEvent::ShutdownComplete { .. } => {
            // clear all channels.
            ctx.connected.take();
            ctx.uni.take();
            ctx.bidi.take();
        }
        _ => {}
    }
    Ok(())
}

impl Connection {
    /// Connects to the server
    pub async fn connect(
        reg: &Registration,
        config: &Configuration,
        server_name: &str,
        server_port: u16,
    ) -> Result<Self, Status> {
        let (mut ctx, mut crx) = conn_ctx_channel();
        let handler =
            move |_: ConnectionRef, ev: ConnectionEvent| connection_callback(&mut ctx, ev);
        let mut conn = msquic::Connection::new();
        conn.open(reg, handler)?;
        conn.start(config, server_name, server_port)?;
        // wait for connection.
        crx.connected
            .take()
            .unwrap()
            .await
            .map_err(|_| Status::new(StatusCode::QUIC_STATUS_ABORTED))?;

        let conn = Arc::new(conn);

        let opener = StreamOpener::new(conn.clone());

        Ok(Self {
            conn,
            ctx: crx,
            opener,
        })
    }

    /// attach to an accepted connection
    pub(crate) fn attach(inner: msquic::Connection) -> Self {
        let (mut ctx, crx) = conn_ctx_channel();
        let handler =
            move |_: ConnectionRef, ev: ConnectionEvent| connection_callback(&mut ctx, ev);
        inner.set_callback_handler(handler);
        let conn = Arc::new(inner);

        let opener = StreamOpener::new(conn.clone());

        Self {
            conn,
            ctx: crx,
            opener,
        }
    }
}

/// responsible for open streams on a connection.
#[derive(Debug)]
pub struct StreamOpener {
    conn: Arc<msquic::Connection>,
    bidi_temp: Option<H3Stream>,
    uni_temp: Option<H3Stream>,
}

impl Clone for StreamOpener {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            bidi_temp: None,
            uni_temp: None,
        }
    }
}

/// Server accept streams
impl<B: Buf> h3::quic::Connection<B> for Connection {
    type RecvStream = H3RecvStream;

    type OpenStreams = StreamOpener;

    type AcceptError = H3Error;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_accept_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Self::RecvStream>, Self::AcceptError>> {
        let s = ready!(self.ctx.uni.poll_recv(cx)).unwrap_or(None);
        // wrap for h3 type. Drop the send stream part
        std::task::Poll::Ready(Ok(s.map(|s| s.recv)))
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_accept_bidi(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Self::BidiStream>, Self::AcceptError>> {
        let s = ready!(self.ctx.bidi.poll_recv(cx)).unwrap_or(None);
        // wrap for h3 type
        std::task::Poll::Ready(Ok(s))
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn opener(&self) -> Self::OpenStreams {
        StreamOpener::new(self.conn.clone())
    }
}

/// Create new streams from connection.
impl<B: Buf> OpenStreams<B> for StreamOpener {
    type BidiStream = H3Stream;

    type SendStream = H3SendStream;

    type OpenError = H3Error;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_open_bidi(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::BidiStream, Self::OpenError>> {
        Self::poll_open_inner(&self.conn, false, &mut self.bidi_temp, cx)
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_open_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::SendStream, Self::OpenError>> {
        let res = ready!(Self::poll_open_inner(
            &self.conn,
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
        self.conn
            .shutdown(ConnectionShutdownFlags::NONE, code.value());
    }
}

impl StreamOpener {
    fn new(conn: Arc<msquic::Connection>) -> Self {
        Self {
            conn,
            bidi_temp: None,
            uni_temp: None,
        }
    }

    /// open a stream and poll it in the holder.
    fn poll_open_inner(
        conn: &Arc<msquic::Connection>,
        uni: bool,
        stream_holder: &mut Option<H3Stream>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<H3Stream, H3Error>> {
        if stream_holder.is_none() {
            // create new stream
            let s = match H3Stream::open_and_start(conn, uni) {
                Ok(s) => s,
                Err(e) => return std::task::Poll::Ready(Err(H3Error::new(e, None))),
            };
            *stream_holder = Some(s);
        }

        // poll stream start.
        let res = {
            let s = stream_holder.as_mut().unwrap();
            let rx = s.send.sctx.start.as_mut().unwrap();
            let p = Pin::new(rx);
            ready!(std::future::Future::poll(p, cx))
        };
        // current stream is either ready or error. So ready to be returned or dropped.
        let s = stream_holder.take().unwrap();
        let res = res
            .expect("cannot receive")
            .map(|_| s)
            .map_err(|e| H3Error::new(e, None));
        std::task::Poll::Ready(res)
    }
}

/// bypass for StreamOpener
impl<B: Buf> OpenStreams<B> for Connection {
    type BidiStream = H3Stream;

    type SendStream = H3SendStream;

    type OpenError = H3Error;

    fn poll_open_bidi(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::BidiStream, Self::OpenError>> {
        OpenStreams::<B>::poll_open_bidi(&mut self.opener, cx)
    }

    fn poll_open_send(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Self::SendStream, Self::OpenError>> {
        OpenStreams::<B>::poll_open_send(&mut self.opener, cx)
    }

    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        OpenStreams::<B>::close(&mut self.opener, code, reason)
    }
}

/// Msquic Stream.
#[derive(Debug)]
pub struct H3Stream {
    send: H3SendStream,
    recv: H3RecvStream,
}
#[derive(Debug)]
pub struct H3SendStream {
    stream: Arc<msquic::Stream>,
    sctx: SendStreamReceiveCtx,
}
#[derive(Debug)]
pub struct H3RecvStream {
    stream: Arc<msquic::Stream>,
    rctx: RecvStreamReceiveCtx,
}

struct BufPtr(*const c_void);
unsafe impl Send for BufPtr {}
unsafe impl Sync for BufPtr {}

struct StreamSendCtx {
    start: Option<oneshot::Sender<Result<(), Status>>>,
    // cancelled, client_context
    send: Option<mpsc::UnboundedSender<(bool, BufPtr)>>,
    shutdown: Option<oneshot::Sender<()>>,
    receive: Option<mpsc::UnboundedSender<BytesMut>>,
}

/// ctx for receiving data on frontend.
#[derive(Debug)]
struct RecvStreamReceiveCtx {
    receive: mpsc::UnboundedReceiver<BytesMut>,
}

/// ctx for sending data on frontend.
#[derive(Debug)]
struct SendStreamReceiveCtx {
    start: Option<oneshot::Receiver<Result<(), Status>>>,
    // cancelled, client_context
    send: mpsc::UnboundedReceiver<(bool, BufPtr)>,
    send_inprogress: bool,
    shutdown: oneshot::Receiver<()>,
}

fn stream_ctx_channel() -> (StreamSendCtx, SendStreamReceiveCtx, RecvStreamReceiveCtx) {
    let (start_tx, start_rx) = oneshot::channel::<Result<(), Status>>();
    let (send_tx, send_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (receive_tx, receive_rx) = mpsc::unbounded_channel();
    (
        StreamSendCtx {
            start: Some(start_tx),
            send: Some(send_tx),
            shutdown: Some(shutdown_tx),
            receive: Some(receive_tx),
        },
        SendStreamReceiveCtx {
            start: Some(start_rx),
            send: send_rx,
            send_inprogress: false,
            shutdown: shutdown_rx,
        },
        RecvStreamReceiveCtx {
            receive: receive_rx,
        },
    )
}

#[cfg_attr(
    feature = "tracing",
    tracing::instrument(skip(ctx), level = "trace", ret)
)]
fn stream_callback(ctx: &mut StreamSendCtx, ev: StreamEvent) -> Result<(), Status> {
    match ev {
        StreamEvent::StartComplete { status, .. } => {
            let tx = ctx.start.take().unwrap();
            if status.is_ok() {
                tx.send(Ok(())).expect("cannot send");
            } else {
                tx.send(Err(status)).expect("cannot send")
            }
        }
        StreamEvent::SendComplete {
            cancelled,
            client_context,
        } => {
            if let Some(send) = ctx.send.as_ref() {
                send.send((cancelled, BufPtr(client_context)))
                    .expect("cannot send");
            } else {
                debug_assert!(false, "mem leak");
            }
        }
        StreamEvent::Receive { buffers, flags, .. } => {
            if let Some(receive) = ctx.receive.as_ref() {
                let mut b = BytesMut::new();
                for br in buffers {
                    // skip empty buffs.
                    if !br.as_bytes().is_empty() {
                        b.put_slice(br.as_bytes());
                    }
                }
                if !b.is_empty() {
                    receive.send(b).expect("cannot send");
                } else {
                    // zero buff can happen. so drop the receiver.
                    ctx.receive.take();
                }
            }
            if flags.contains(ReceiveFlags::FIN) {
                // close
                ctx.receive.take();
            }
        }
        StreamEvent::SendShutdownComplete { graceful: _ } => {
            // Peer acknowledged shutdown.
            if let Some(shutdown) = ctx.shutdown.take() {
                shutdown.send(()).expect("cannot send");
            }
        }
        StreamEvent::ShutdownComplete { .. } => {
            // close all channels
            ctx.receive.take();
            ctx.send.take();
            ctx.shutdown.take();
            ctx.start.take();
        }
        _ => {}
    }
    Ok(())
}

impl H3Stream {
    /// attach to accepted stream
    pub(crate) fn attach(stream: msquic::Stream) -> Self {
        let (mut ctx, rtx, rrtx) = stream_ctx_channel();
        let handler = move |_: StreamRef, ev: StreamEvent| stream_callback(&mut ctx, ev);

        stream.set_callback_handler(handler);
        let s = Arc::new(stream);
        Self {
            send: H3SendStream {
                stream: s.clone(),
                sctx: rtx,
            },
            recv: H3RecvStream {
                stream: s,
                rctx: rrtx,
            },
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", err, ret)
    )]
    fn open_and_start(conn: &msquic::Connection, uni: bool) -> Result<Self, Status> {
        let (mut ctx, rtx, rrtx) = stream_ctx_channel();
        let handler = move |_: StreamRef, ev: StreamEvent| stream_callback(&mut ctx, ev);

        let flag = match uni {
            true => StreamOpenFlags::UNIDIRECTIONAL,
            false => StreamOpenFlags::NONE,
        };

        let mut s = msquic::Stream::new();
        s.open(conn, flag, handler)?;
        s.start(StreamStartFlags::NONE)?;
        let s = Arc::new(s);
        Ok(Self {
            send: H3SendStream {
                stream: s.clone(),
                sctx: rtx,
            },
            recv: H3RecvStream {
                stream: s,
                rctx: rrtx,
            },
        })
    }
}

impl<B: Buf> SendStream<B> for H3SendStream {
    type Error = H3Error;

    // Seems like poll_ready is called after send_data is called.
    // To ensure data is sent.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        if !self.sctx.send_inprogress {
            // no send is current so ready to get more.
            return std::task::Poll::Ready(Ok(()));
        }
        match ready!(self.sctx.send.poll_recv(cx)) {
            Some((cancelled, ptr)) => {
                self.sctx.send_inprogress = false;
                // reattach buff
                let _: H3Buff<h3::quic::WriteBuf<B>> =
                    unsafe { H3Buff::from_raw(ptr.0 as *mut c_void) };
                match cancelled {
                    true => std::task::Poll::Ready(Err(H3Error::new(
                        Status::from(StatusCode::QUIC_STATUS_ABORTED),
                        None,
                    ))),
                    false => std::task::Poll::Ready(Ok(())),
                }
            }
            // closed.
            None => std::task::Poll::Ready(Err(H3Error::new(
                Status::from(StatusCode::QUIC_STATUS_ABORTED),
                None,
            ))),
        }
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret, err)
    )]
    fn send_data<T: Into<h3::quic::WriteBuf<B>>>(&mut self, data: T) -> Result<(), Self::Error> {
        if self.sctx.send_inprogress {
            panic!("send while send is in progress.");
        }
        let data: h3::quic::WriteBuf<B> = data.into();
        let buff = H3Buff::new(data);
        let (buff_ref, ptr) = unsafe { buff.into_raw() };
        unsafe { self.stream.send(buff_ref, SendFlags::NONE, ptr) }
            .inspect_err(|_| {
                // reattach buff
                let _: H3Buff<h3::quic::WriteBuf<B>> = unsafe { H3Buff::from_raw(ptr) };
            })
            .map_err(|e| H3Error::new(e, None))?;
        self.sctx.send_inprogress = true;
        Ok(())
    }

    // Send FIN signal to peer.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_finish(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        // Graceful sends a Fin to peer.
        if let Err(e) = self.stream.shutdown(StreamShutdownFlags::GRACEFUL, 0) {
            return std::task::Poll::Ready(Err(H3Error::new(e, None)));
        }
        // poll the ctx
        let rx = &mut self.sctx.shutdown;
        let p = Pin::new(rx);
        // if backend is closed return error.
        let res = ready!(std::future::Future::poll(p, cx))
            .map_err(|_| H3Error::new(Status::from(StatusCode::QUIC_STATUS_ABORTED), None));
        std::task::Poll::Ready(res)
    }

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn reset(&mut self, _reset_code: u64) {
        panic!("reset not supported")
    }

    fn send_id(&self) -> h3::quic::StreamId {
        get_id(&self.stream)
    }
}

fn get_id(s: &msquic::Stream) -> h3::quic::StreamId {
    let raw_id = unsafe {
        msquic::Api::get_param_auto::<u64>(s.as_raw(), msquic::ffi::QUIC_PARAM_STREAM_ID)
    }
    .unwrap();
    raw_id.try_into().expect("cannot parse id")
}

impl RecvStream for H3RecvStream {
    type Buf = BytesMut;

    type Error = H3Error;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn poll_data(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Self::Buf>, Self::Error>> {
        let res = ready!(self.rctx.receive.poll_recv(cx));
        std::task::Poll::Ready(Ok(res))
    }

    /// Stop accepting data. Discard unread data, notify peer to not send.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn stop_sending(&mut self, error_code: u64) {
        // Close the send path.
        let _ = self
            .stream
            .shutdown(StreamShutdownFlags::ABORT_RECEIVE, error_code);
    }

    fn recv_id(&self) -> h3::quic::StreamId {
        get_id(&self.stream)
    }
}

// bidi stream

impl<B: Buf> SendStream<B> for H3Stream {
    type Error = H3Error;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        SendStream::<B>::poll_ready(&mut self.send, cx)
    }

    fn send_data<T: Into<h3::quic::WriteBuf<B>>>(&mut self, data: T) -> Result<(), Self::Error> {
        SendStream::<B>::send_data(&mut self.send, data)
    }

    fn poll_finish(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        SendStream::<B>::poll_finish(&mut self.send, cx)
    }

    fn reset(&mut self, reset_code: u64) {
        SendStream::<B>::reset(&mut self.send, reset_code);
    }

    fn send_id(&self) -> h3::quic::StreamId {
        SendStream::<B>::send_id(&self.send)
    }
}

impl RecvStream for H3Stream {
    type Buf = BytesMut;

    type Error = H3Error;

    fn poll_data(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<Option<Self::Buf>, Self::Error>> {
        RecvStream::poll_data(&mut self.recv, cx)
    }

    fn stop_sending(&mut self, error_code: u64) {
        RecvStream::stop_sending(&mut self.recv, error_code)
    }

    fn recv_id(&self) -> h3::quic::StreamId {
        RecvStream::recv_id(&self.recv)
    }
}

impl<B: Buf> BidiStream<B> for H3Stream {
    type SendStream = H3SendStream;

    type RecvStream = H3RecvStream;

    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(skip_all, level = "trace", ret)
    )]
    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

#[cfg(test)]
mod test {
    use bytes::Buf;
    use http::Uri;
    use msquic::{
        BufferRef, Configuration, CredentialConfig, CredentialFlags, Registration,
        RegistrationConfig, Settings,
    };

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
                .args(["-Command", "Get-ChildItem Cert:\\CurrentUser\\My | Where-Object -Property FriendlyName -EQ -Value MsQuicTestServer | Select-Object -ExpandProperty Thumbprint -First 1"]).
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
                    .current_dir(cert_dir)
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
        let reg = Registration::new(&config).unwrap();

        let alpn = BufferRef::from("h3");
        // create an client
        // open client
        let client_settings = Settings::new().set_IdleTimeoutMs(2000);
        let client_config = Configuration::new(&reg, &[alpn], Some(&client_settings)).unwrap();
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
            std::future::poll_fn(|cx| driver.poll_close(cx)).await?;
            Ok::<(), Box<dyn std::error::Error>>(())
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
    }

    #[test]
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
