use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use msquic::{BufferRef, Configuration, RegistrationConfig, Settings, Status};

/// Wakers for all outstanding [`WaitIdle`] futures.
#[derive(Debug, Default)]
struct Waiters {
    next: u64,
    map: HashMap<u64, Waker>,
}

/// Shared rundown-tracking state, owned by a [`Registration`] and cloned into
/// every tracked handle (connections and listeners).
#[derive(Debug, Default)]
pub(crate) struct RundownState {
    /// Outstanding handle reservations plus live tracked handles
    /// (connections + listeners). Reserved eagerly, so it conservatively
    /// over-counts relative to the native `Registration->Rundown`. Decremented
    /// when the handle's Close runs (or a reservation is abandoned).
    active: AtomicUsize,
    /// Wakers for all outstanding `wait_idle` futures. Drained and woken when
    /// `active` transitions to 0. Supports any number of concurrent waiters.
    waiters: Mutex<Waiters>,
}

/// RAII guard that holds one unit of a [`RundownState`]'s `active` count.
///
/// Store it as the field declared *after* the msquic handle it guards so that,
/// on drop, the handle's Close (which releases the native registration rundown
/// ref) runs before this guard decrements and wakes waiters.
#[derive(Debug)]
pub(crate) struct RundownGuard {
    state: Arc<RundownState>,
}

impl RundownGuard {
    pub(crate) fn new(state: Arc<RundownState>) -> Self {
        state.active.fetch_add(1, Ordering::Relaxed);
        Self { state }
    }
}

impl Drop for RundownGuard {
    fn drop(&mut self) {
        // Release so the Close that just ran (on the handle field declared
        // before this guard) is visible to a thread that later observes
        // active == 0 with Acquire.
        if self.state.active.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            // Wake every outstanding waiter. Collect under the lock, wake after
            // releasing it.
            let woken: Vec<Waker> = {
                let mut w = crate::lock_recover(&self.state.waiters);
                w.map.drain().map(|(_, waker)| waker).collect()
            };
            for waker in woken {
                waker.wake();
            }
        }
    }
}

/// msquic `Registration` wrapper that tracks live connections and listeners so
/// teardown can wait for the rundown to drain before the blocking
/// `RegistrationClose`.
///
/// The raw handle is intentionally not exposed publicly: every rundown-holding
/// handle must be created through a controlled entry point
/// ([`crate::Connection::connect`], [`crate::Listener::new`],
/// [`Registration::open_configuration`]) so the count can never be bypassed.
pub struct Registration {
    inner: msquic::Registration,
    state: Arc<RundownState>,
}

impl Registration {
    pub fn new(config: &RegistrationConfig) -> Result<Self, Status> {
        Ok(Self {
            inner: msquic::Registration::new(config)?,
            state: Arc::new(RundownState::default()),
        })
    }

    /// Queue shutdown to all connections and stop all listeners. Non-blocking.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }

    /// Open a configuration against this registration.
    ///
    /// Untracked by design: the returned `Configuration` must be dropped after
    /// [`Registration::wait_idle`] has resolved and before the `Registration`.
    pub fn open_configuration(
        &self,
        alpn: &[BufferRef],
        settings: Option<&Settings>,
    ) -> Result<Configuration, Status> {
        Configuration::open(&self.inner, alpn, settings)
    }

    /// Resolves once the registration rundown holds no msquic-h3 connection or
    /// listener. Call after [`Registration::shutdown`] (and after dropping any
    /// [`crate::Listener`]), before dropping configurations and the
    /// registration.
    pub fn wait_idle(&self) -> WaitIdle {
        WaitIdle {
            state: self.state.clone(),
            key: None,
        }
    }

    /// Raw handle for crate-internal constructors only.
    pub(crate) fn raw(&self) -> &msquic::Registration {
        &self.inner
    }

    pub(crate) fn state(&self) -> &Arc<RundownState> {
        &self.state
    }
}

/// Future returned by [`Registration::wait_idle`]. Resolves when every tracked
/// connection and listener has released the rundown.
pub struct WaitIdle {
    state: Arc<RundownState>,
    /// Slot in [`RundownState::waiters`], allocated on the first `Pending` poll.
    key: Option<u64>,
}

impl WaitIdle {
    fn deregister(&mut self) {
        if let Some(k) = self.key.take() {
            crate::lock_recover(&self.state.waiters).map.remove(&k);
        }
    }
}

impl Future for WaitIdle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        // Fast path.
        if this.state.active.load(Ordering::Acquire) == 0 {
            this.deregister();
            return Poll::Ready(());
        }
        // Register/refresh this task's waker under the lock.
        {
            let mut w = crate::lock_recover(&this.state.waiters);
            match this.key {
                Some(k) => {
                    w.map.insert(k, cx.waker().clone());
                }
                None => {
                    let k = w.next;
                    w.next += 1;
                    w.map.insert(k, cx.waker().clone());
                    this.key = Some(k);
                }
            }
        }
        // Re-check after registering to avoid a lost wakeup against a guard that
        // drained to zero between the fast-path load and taking the lock.
        if this.state.active.load(Ordering::Acquire) == 0 {
            this.deregister();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for WaitIdle {
    fn drop(&mut self) {
        self.deregister();
    }
}

#[cfg(test)]
mod test {
    use std::{
        sync::Arc,
        task::{Context, Poll},
    };

    use futures::task::noop_waker;

    use super::{RundownGuard, RundownState, WaitIdle};

    fn wait_idle(state: &Arc<RundownState>) -> WaitIdle {
        WaitIdle {
            state: state.clone(),
            key: None,
        }
    }

    fn poll(fut: &mut WaitIdle) -> Poll<()> {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        std::pin::Pin::new(fut).poll(&mut cx)
    }

    #[test]
    fn idle_from_start_resolves_immediately() {
        let state = Arc::new(RundownState::default());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }

    #[test]
    fn reservation_keeps_pending_until_dropped() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Pending);
        drop(guard);
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }

    #[test]
    fn guard_drop_wakes_registered_waiter() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct FlagWaker(Arc<AtomicBool>);
        impl std::task::Wake for FlagWaker {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);

        let woken = Arc::new(AtomicBool::new(false));
        let waker = std::task::Waker::from(Arc::new(FlagWaker(woken.clone())));
        let mut cx = Context::from_waker(&waker);
        assert_eq!(std::pin::Pin::new(&mut fut).poll(&mut cx), Poll::Pending);
        assert!(!woken.load(Ordering::SeqCst));

        drop(guard);
        assert!(woken.load(Ordering::SeqCst), "waiter should be woken");
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }

    #[test]
    fn concurrent_waiters_all_complete() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut a = wait_idle(&state);
        let mut b = wait_idle(&state);
        assert_eq!(poll(&mut a), Poll::Pending);
        assert_eq!(poll(&mut b), Poll::Pending);
        drop(guard);
        assert_eq!(poll(&mut a), Poll::Ready(()));
        assert_eq!(poll(&mut b), Poll::Ready(()));
    }

    #[test]
    fn dropped_waiter_deregisters_its_slot() {
        let state = Arc::new(RundownState::default());
        let guard = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Pending);
        assert_eq!(state.waiters.lock().unwrap().map.len(), 1);
        drop(fut);
        assert_eq!(
            state.waiters.lock().unwrap().map.len(),
            0,
            "slot must be removed on drop"
        );
        drop(guard);
    }

    #[test]
    fn nested_reservations_need_all_released() {
        let state = Arc::new(RundownState::default());
        let g1 = RundownGuard::new(state.clone());
        let g2 = RundownGuard::new(state.clone());
        let mut fut = wait_idle(&state);
        assert_eq!(poll(&mut fut), Poll::Pending);
        drop(g1);
        assert_eq!(poll(&mut fut), Poll::Pending);
        drop(g2);
        assert_eq!(poll(&mut fut), Poll::Ready(()));
    }
}
