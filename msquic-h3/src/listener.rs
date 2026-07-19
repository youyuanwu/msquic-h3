use std::{
    cell::Cell,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::ready;
use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use msquic::{BufferRef, ListenerEvent, ListenerRef, Status};

use crate::registration::{RundownGuard, RundownState};
use crate::{
    CbClass, NoShutdown, PoisonFlag, ShutdownSeam, guard_callback, internal_error_status,
    report_contained_panic,
};

pub struct Listener {
    inner: msquic::Listener,
    conn: ListenerCtxReceiver,
    // The listener-wide `H3Config` is not stored here: it is captured by `Copy`
    // into the accept callback closure (see `with_config`), which is the single
    // source of truth passed to every accepted `Connection::attach`. Keeping a
    // second copy on the struct would be dead, drift-prone state.
    // Dropped last: `inner`'s ListenerClose releases the native rundown ref
    // before this guard decrements and wakes `wait_idle` waiters.
    _guard: RundownGuard,
}

struct ListenerCtxSender {
    conn: Option<mpsc::UnboundedSender<Option<crate::Connection>>>,
    shutdown: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    /// Set by [`listener_recover`] after a contained callback panic (SF-E), so
    /// [`guard_callback`] short-circuits every subsequent event on this ctx.
    /// Interior-mutable because the listener callback is an `Fn` closure holding
    /// the ctx by shared reference.
    ///
    /// NOTE (F-B): connection ownership state is deliberately NOT stored here.
    /// msquic runs listener callbacks IN PARALLEL, so a shared ownership bit
    /// would race across concurrent invocations. Ownership is tracked in a
    /// per-invocation `Cell<bool>` created inside the handler instead.
    poisoned: AtomicBool,
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
            poisoned: AtomicBool::new(false),
        },
        ListenerCtxReceiver {
            conn: rx,
            shutdown: std::sync::Mutex::new(Some(sh_rx)),
        },
    )
}

impl PoisonFlag for &ListenerCtxSender {
    fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Relaxed)
    }
}

/// Event-aware poison disposition for the listener callback (SF-E). Once the ctx
/// is poisoned, a `NewConnection` (ownership-bearing) is rejected with
/// `INTERNAL_ERROR` so msquic performs the single reject without the adapter
/// attempting `from_raw`; a `StopComplete` (teardown) is a safe `Ok(())` no-op.
fn listener_poison_disp(ev: &ListenerEvent) -> Result<(), Status> {
    match ev {
        ListenerEvent::StopComplete { .. } => Ok(()),
        _ => Err(internal_error_status()),
    }
}

/// Ownership-aware panic-recovery action for the listener callback (SF-E). The
/// listener never force-closes (its seam is a no-op): it mirrors the
/// `ConfigFailedClose` close-or-reject contract. If ownership was already taken
/// this invocation (`owned.get() == true`), the unwind's `Drop` already closed
/// the owned connection, so returning `Ok(())` prevents msquic from also
/// rejecting it (a double-close). Otherwise ownership never transferred, so
/// returning `Err(INTERNAL_ERROR)` lets msquic perform the single reject. Either
/// branch yields exactly one close/reject with no leaked handle. Marks the ctx
/// poisoned.
///
/// `owned` is a PER-INVOCATION token (F-B): msquic runs listener callbacks in
/// parallel, so ownership must never be read from shared ctx state — a fresh
/// `Cell<bool>` is created in the handler and captured by both the body and this
/// recover closure, so concurrent invocations cannot corrupt each other's
/// close-or-reject decision.
///
/// It ALSO wakes both listener terminal waiters — mirroring [`StopComplete`
/// handling in `listener_callback`] — so a pending `accept()` and a pending
/// `shutdown()` cannot hang after the panic. This is essential because once the
/// ctx is poisoned a later `StopComplete` is short-circuited to a no-op `Ok(())`
/// by [`listener_poison_disp`] and would never signal them.
fn listener_recover(
    ctx: &mut &ListenerCtxSender,
    _seam: &dyn ShutdownSeam,
    owned: &Cell<bool>,
) -> Result<(), Status> {
    report_contained_panic(CbClass::Listener);
    let ownership_taken = owned.get();
    // Wake both terminal paths so no waiter hangs: signal end-of-connections to
    // any pending `accept()`, and resolve the `shutdown()`/StopComplete waiter.
    // Mirrors the `StopComplete` arm of `listener_callback`.
    if let Some(tx) = ctx.conn.as_ref() {
        let _ = tx.unbounded_send(None);
    }
    let sh = crate::lock_recover(&ctx.shutdown).take();
    if let Some(sh) = sh {
        let _ = sh.send(());
    }
    ctx.poisoned.store(true, Ordering::Relaxed);
    if ownership_taken {
        // Ownership already transferred; the unwind's Drop closed the owned
        // connection. Do NOT also reject (avoids the native double-close assert).
        Ok(())
    } else {
        // Ownership never transferred; msquic performs the single reject/close.
        Err(internal_error_status())
    }
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
    owned: &Cell<bool>,
    h3config: crate::H3Config,
) -> Result<(), Status> {
    // `owned` is a PER-INVOCATION token (F-B); it starts `false` for every
    // callback (fresh `Cell` per invocation in the handler), so there is no
    // stale prior state to reset and no shared bit that a parallel callback
    // could race.
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
                // `set_configuration` succeeded: only NOW take ownership. Mark the
                // per-invocation token the instant it transfers, so a subsequent
                // panic's `listener_recover` knows the unwind's Drop already closed
                // the owned connection (and must not also reject it).
                let inner = unsafe { msquic::Connection::from_raw(connection.as_raw()) };
                owned.set(true);
                let conn = crate::Connection::attach(inner, guard, h3config);
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
    /// Create a listener using the default memory budgets ([`H3Config::default`]).
    ///
    /// [`H3Config::default`]: crate::H3Config::default
    pub fn new(
        reg: &crate::Registration,
        config: Arc<msquic::Configuration>,
        alpn: &[BufferRef],
        local_addr: Option<SocketAddr>,
    ) -> Result<Self, Status> {
        Self::with_config(reg, config, alpn, local_addr, crate::H3Config::default())
    }

    /// Create a listener with an explicit [`H3Config`](crate::H3Config),
    /// applied to every connection it accepts (and thus to every stream those
    /// connections open/accept).
    pub fn with_config(
        reg: &crate::Registration,
        config: Arc<msquic::Configuration>,
        alpn: &[BufferRef],
        local_addr: Option<SocketAddr>,
        h3config: crate::H3Config,
    ) -> Result<Self, Status> {
        let (tx, rx) = listener_ctx_channel();
        let state = reg.state().clone();
        let handler = move |_: ListenerRef, ev: ListenerEvent| {
            let disp = listener_poison_disp(&ev);
            // `C = &ListenerCtxSender`: the listener callback is an `Fn` closure,
            // so the ctx is held (and mutated via interior atomics) through a
            // shared reference. `guard_callback` still takes `&mut C` uniformly.
            let mut ctx_ref: &ListenerCtxSender = &tx;
            // Per-invocation ownership token (F-B): a FRESH `Cell` for every
            // callback, so parallel listener callbacks never share ownership
            // state. Captured by both the body (sets it after `from_raw`) and the
            // recover closure (reads it).
            let owned = Cell::new(false);
            guard_callback(
                &mut ctx_ref,
                &NoShutdown,
                disp,
                |c| listener_callback(c, ev, &config, &state, &owned, h3config),
                |c, seam| listener_recover(c, seam, &owned),
            )
        };
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

/// FFI callback panic containment (SF-E / FR-007) for the listener class.
/// HERMETIC: a native `ConnectionRef` cannot be forged, so the ownership-aware
/// [`listener_recover`] is exercised through [`guard_callback`] with a body that
/// mirrors [`listener_callback`]'s `ownership_taken` handling (reset at the start
/// of the invocation, set the instant `from_raw` would take ownership). The
/// listener never force-closes, so `Err(INTERNAL_ERROR)` models msquic performing
/// exactly one reject and `Ok(())` models the owned-connection `Drop` performing
/// the single close (never both — no double-close, no leak).
#[cfg(test)]
mod callback_safety {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use msquic::{ListenerEvent, Status, StatusCode};

    use super::{
        ListenerCtxReceiver, ListenerCtxSender, listener_ctx_channel, listener_poison_disp,
        listener_recover,
    };
    use crate::{NoShutdown, guard_callback};

    fn is_internal_err(r: &Result<(), Status>) -> bool {
        matches!(r, Err(s) if s.0 == Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR).0)
    }

    /// RAII stand-in for the owned `Connection` that `from_raw` hands over. Its
    /// `Drop` increments a shared counter, mirroring how the real owned
    /// `Connection`'s `Drop` performs the single native close. Constructing one
    /// models taking ownership; the unwind dropping it models the close — so the
    /// close becomes directly OBSERVABLE (counter), not merely inferred from the
    /// recover return value.
    struct OwnedStandIn(Arc<AtomicUsize>);
    impl Drop for OwnedStandIn {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Assert both listener terminal waiters were signalled by `listener_recover`,
    /// so a pending `accept()` and a pending `shutdown()` cannot hang after a
    /// contained panic. `accept()` must observe the end-of-connections sentinel
    /// (`None`) and `shutdown()`'s one-shot must be resolved.
    fn assert_both_waiters_woken(rx: &mut ListenerCtxReceiver) {
        match rx.conn.try_recv() {
            Ok(None) => {}
            Ok(Some(_)) => panic!("accept received a connection, expected end sentinel"),
            Err(_) => panic!("accept waiter not woken (channel empty) — would hang"),
        }
        let mut lk = crate::lock_recover(&rx.shutdown);
        let sh = lk.as_mut().expect("shutdown receiver present");
        match sh.try_recv() {
            Ok(Some(())) => {}
            Ok(None) => panic!("shutdown waiter not resolved — would hang"),
            Err(_) => panic!("shutdown sender dropped without resolving"),
        }
    }

    #[test]
    fn listener_callback_panic_before_from_raw_rejects() {
        let (tx, mut rx) = listener_ctx_channel();
        // Close counter: no owned stand-in is ever created on this path.
        let closes = Arc::new(AtomicUsize::new(0));
        // Per-invocation ownership token (F-B): local, never shared with the ctx.
        let owned = Cell::new(false);
        let mut ctx_ref: &ListenerCtxSender = &tx;
        let got = guard_callback(
            &mut ctx_ref,
            &NoShutdown,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| {
                // Ownership never transferred, so no `OwnedStandIn` is constructed
                // (from_raw never runs) and `owned` stays false.
                let _keep_alive = &closes;
                // Panic BEFORE taking ownership (from_raw never runs).
                panic!("boom before from_raw");
            },
            |c, seam| listener_recover(c, seam, &owned),
        );
        // Ownership never transferred → recover returns Err: msquic performs the
        // single reject (reject == 1), and ZERO owned-connection close happened.
        assert!(is_internal_err(&got), "the single reject is the Err return");
        assert_eq!(
            closes.load(Ordering::Relaxed),
            0,
            "no owned stand-in exists, so zero close occurs (msquic rejects instead)"
        );
        assert!(tx.poisoned.load(Ordering::Relaxed));
        // Both terminal waiters are woken so accept()/shutdown() cannot hang.
        assert_both_waiters_woken(&mut rx);
    }

    #[test]
    fn listener_callback_panic_after_from_raw_does_not_reject() {
        let (tx, mut rx) = listener_ctx_channel();
        // Close counter: the owned stand-in's Drop increments it exactly once.
        let closes = Arc::new(AtomicUsize::new(0));
        let owned = Cell::new(false);
        let mut ctx_ref: &ListenerCtxSender = &tx;
        let got = guard_callback(
            &mut ctx_ref,
            &NoShutdown,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| {
                // Emulate from_raw taking ownership: construct the owned stand-in
                // and mark the per-invocation token the instant it transfers. The
                // named binding lives until the panic, so the unwind's Drop
                // performs the single close (counter += 1).
                let _owned_standin = OwnedStandIn(closes.clone());
                owned.set(true);
                panic!("boom after from_raw");
            },
            |c, seam| listener_recover(c, seam, &owned),
        );
        // Ownership already taken + dropped → msquic must NOT reject (Ok): the
        // owned-connection Drop performed the single close (no double-close).
        assert!(got.is_ok(), "recover returns Ok so msquic does NOT reject");
        assert_eq!(
            closes.load(Ordering::Relaxed),
            1,
            "the owned stand-in's Drop performs exactly one close"
        );
        assert!(tx.poisoned.load(Ordering::Relaxed));
        // Even on the owned-connection close path, both terminal waiters are woken
        // so accept()/shutdown() cannot hang.
        assert_both_waiters_woken(&mut rx);
    }

    /// F-B: the close-or-reject decision is INVOCATION-LOCAL. msquic runs listener
    /// callbacks in PARALLEL, one per accepted connection, so ownership must be
    /// tracked in a per-call token, never a shared ctx bit. This models two
    /// concurrent callbacks with TWO SEPARATE listener contexts (mirroring
    /// msquic's real per-connection parallel callbacks) so neither invocation's
    /// poison can short-circuit the other: invocation A takes ownership then
    /// panics → Ok / exactly one close; invocation B panics pre-ownership → Err /
    /// exactly one reject / zero close. BOTH recovery paths genuinely run (each
    /// ctx is poisoned only by its own `listener_recover`), and each keeps its own
    /// decision with zero cross-talk. If ownership were a shared bit, A's set or
    /// B's reset would corrupt the other's outcome.
    #[test]
    fn listener_ownership_decision_is_invocation_local() {
        // Two INDEPENDENT contexts: A's recovery poisons only ctx A, so B's
        // guard_callback poison check never short-circuits B — both bodies panic
        // and both recovery paths run to completion.
        let (tx_a, _rx_a) = listener_ctx_channel();
        let (tx_b, _rx_b) = listener_ctx_channel();

        // Independent per-invocation tokens and close counters.
        let owned_a = Cell::new(false);
        let owned_b = Cell::new(false);
        let closes_a = Arc::new(AtomicUsize::new(0));
        let closes_b = Arc::new(AtomicUsize::new(0));

        // Invocation A: takes ownership (from_raw ran) then panics → its owned
        // stand-in Drop performs the single close.
        let mut ctx_a: &ListenerCtxSender = &tx_a;
        let got_a = guard_callback(
            &mut ctx_a,
            &NoShutdown,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| {
                let _owned_standin = OwnedStandIn(closes_a.clone());
                owned_a.set(true);
                panic!("A panics after ownership");
            },
            |c, seam| listener_recover(c, seam, &owned_a),
        );

        // Invocation B: never takes ownership, then panics → msquic performs the
        // single reject and no owned stand-in is ever constructed (close == 0).
        let mut ctx_b: &ListenerCtxSender = &tx_b;
        let got_b = guard_callback(
            &mut ctx_b,
            &NoShutdown,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| {
                let _keep_alive = &closes_b;
                panic!("B panics before ownership");
            },
            |c, seam| listener_recover(c, seam, &owned_b),
        );

        // BOTH recovery paths actually executed: `poisoned` is set ONLY inside
        // `listener_recover`, so each ctx being poisoned proves its recover ran
        // (B was NOT masked by A's poison).
        assert!(
            tx_a.poisoned.load(Ordering::Relaxed),
            "A's recover ran (ctx A poisoned)"
        );
        assert!(
            tx_b.poisoned.load(Ordering::Relaxed),
            "B's recover ran (ctx B poisoned) — not short-circuited by A"
        );

        // A: Ok (no reject) + exactly one close via the owned Drop, zero rejects.
        assert!(got_a.is_ok(), "A took ownership → Ok (no msquic reject)");
        assert_eq!(
            closes_a.load(Ordering::Relaxed),
            1,
            "A's owned stand-in closed exactly once"
        );
        // B: Err (single reject) + zero close — B saw its OWN false token, not A's.
        assert!(
            is_internal_err(&got_b),
            "B never took ownership → Err (single msquic reject), unaffected by A"
        );
        assert_eq!(
            closes_b.load(Ordering::Relaxed),
            0,
            "B constructed no owned stand-in, so zero close (msquic rejects instead)"
        );

        // Tokens are independent: neither invocation mutated the other's.
        assert!(owned_a.get(), "A token stayed true");
        assert!(!owned_b.get(), "B token stayed false");
    }

    #[test]
    fn listener_poison_short_circuit_is_event_aware() {
        let (tx, _rx) = listener_ctx_channel();
        tx.poisoned.store(true, Ordering::Relaxed);
        let owned = Cell::new(false);
        let mut ctx_ref: &ListenerCtxSender = &tx;

        // Teardown (StopComplete) → Ok, body not run.
        let teardown = ListenerEvent::StopComplete {
            app_close_in_progress: false,
        };
        assert!(listener_poison_disp(&teardown).is_ok());
        let got = guard_callback(
            &mut ctx_ref,
            &NoShutdown,
            listener_poison_disp(&teardown),
            |_c| panic!("body must NOT run when poisoned"),
            |c, seam| listener_recover(c, seam, &owned),
        );
        assert!(got.is_ok());

        // Ownership-bearing / non-teardown disposition → Err, body not run.
        // `NewConnection` carries a native `ConnectionRef` and cannot be forged
        // hermetically; its Err mapping is the `_ => Err` catch-all modeled here.
        let got = guard_callback(
            &mut ctx_ref,
            &NoShutdown,
            Err(Status::new(StatusCode::QUIC_STATUS_INTERNAL_ERROR)),
            |_c| panic!("body must NOT run when poisoned"),
            |c, seam| listener_recover(c, seam, &owned),
        );
        assert!(is_internal_err(&got));
    }

    #[test]
    fn listener_teardown_body_panic_is_contained_and_wakes_waiters() {
        // A panic while handling a TEARDOWN event (StopComplete) is contained:
        // recover wakes both terminal waiters and poisons the ctx, so a pending
        // accept()/shutdown() cannot hang. A subsequent StopComplete then
        // short-circuits to a no-op Ok without re-running the body.
        let (tx, mut rx) = listener_ctx_channel();
        let owned = Cell::new(false);
        let mut ctx_ref: &ListenerCtxSender = &tx;
        let teardown = ListenerEvent::StopComplete {
            app_close_in_progress: false,
        };
        // First event: ctx not yet poisoned, so the body runs and panics.
        let got = guard_callback(
            &mut ctx_ref,
            &NoShutdown,
            listener_poison_disp(&teardown),
            |_c| panic!("boom while handling StopComplete"),
            |c, seam| listener_recover(c, seam, &owned),
        );
        // No ownership taken during teardown → recover returns Err (harmless for a
        // teardown, whose return msquic ignores). The key guarantees follow.
        assert!(is_internal_err(&got));
        assert!(tx.poisoned.load(Ordering::Relaxed));
        // Both terminal waiters woken so nothing hangs after a teardown-body panic.
        assert_both_waiters_woken(&mut rx);

        // A subsequent StopComplete short-circuits to a no-op Ok without running
        // the body (poison short-circuit), so waiters are not signalled twice.
        let got = guard_callback(
            &mut ctx_ref,
            &NoShutdown,
            listener_poison_disp(&teardown),
            |_c| panic!("body must NOT run on teardown after poison"),
            |c, seam| listener_recover(c, seam, &owned),
        );
        assert!(got.is_ok());
    }
}
