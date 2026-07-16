# Design: `Registration::wait_idle()` for msquic-h3

Executor-agnostic registration-idle tracking that lets client (and server) code
tear down a `Registration` safely without the app-side `H3MsQuicClientWaiter`
shim.

Companion to the analysis in
`tonic-h3/docs/msquic-registration-shutdown.md`. This document specifies the
*correct* implementation and, in particular, fixes the one flaw in that
analysis's recommendation.

## The constraint (recap)

Every msquic object opened against a `Registration` acquires the **same**
`Registration->Rundown` reference, and `RegistrationClose` (run from
`drop(msquic::Registration)`) **synchronously blocks** on
`CxPlatRundownReleaseAndWait` until all of them are released. In the pinned
`msquic` 2.5.1-beta core:

- **Connections** — `CxPlatRundownAcquire(&Registration->Rundown)` in
  `QuicConnRegister` (connection.c:501), released in `QuicConnUnregister`
  (connection.c:395/479/524), which runs from `ConnectionClose`.
- **Listeners** — acquired in `QuicListenerOpen` (listener.c:81), released in
  `QuicListenerFree` (listener.c:148), which runs from `ListenerClose`.
- **Configurations** — acquired in `QuicConfigurationOpen` (configuration.c:199),
  released in `QuicConfigurationRelease` (configuration.c:260) when the config's
  refcount hits zero. `ConfigurationClose` only drops the *application's* ref; a
  connection holds its own ref (`QuicConfigurationAddRef`, connection.c:2457),
  so the native config — and its rundown ref — can outlive `ConfigurationClose`
  until every using connection is cleaned up (connection.c:372).

`RegistrationClose` waits on that one rundown (registration.c:175), and
`RegistrationShutdown` only *shuts down* connections and *stops* listeners
(registration.c:253-258) — it closes nothing. So a live connection, listener, or
configuration each independently keeps `drop(reg)` blocked. The safe teardown
order is:

```
reg.shutdown();          // async, non-blocking: queues conn shutdown, stops listeners
wait_until_tracked_handles_closed().await;   // connections + listeners
drop(config);            // must precede drop(reg) — see Scope
drop(reg);               // RegistrationClose now returns without blocking
```

We need a `futures`-based signal for "tracked handles closed" that does not pull in tokio
(msquic-h3 runtime deps are `bytes`, `futures` `["std"]`, `h3`, `msquic`,
optional `tracing` — see [Cargo.toml](../Cargo.toml)).

## API compatibility

This design **intentionally breaks API compatibility** (acceptable at 0.0.x). It
stops re-exporting `msquic::Registration` as the primary type and replaces it
with a wrapper that exposes a deliberately minimal surface. We do **not** add a
`Deref` to (or a public accessor for) the raw registration: doing so would let
callers open connections or listeners that bypass rundown tracking — and would
re-expose the raw handle whose `drop` is the blocking `RegistrationClose`. Every
rundown-holding handle must be created through a controlled wrapper entry point
(`Connection::connect`, `Listener::new`, `Registration::open_configuration`), so
the count can never be silently bypassed. (`open_configuration` is controlled but
*not* tracked — configurations are deliberately excluded from `active`; see
[Scope](#scope-connections-and-listeners-tracked-configurations-are-manual).)
The raw handle is available only `pub(crate)` for those constructors.

## The correction: decrement on handle close, not on wrapper drop

The analysis doc proposed incrementing on connect and decrementing in
`Drop for Connection`. That is **incorrect** for this codebase, because the
msquic handle is shared behind an `Arc` and `Drop for Connection` does *not*
run `ConnectionClose`.

Today the handle lives in `Arc<msquic::Connection>` and is shared by:

- `Connection.conn` — [lib.rs](../msquic-h3/src/lib.rs) line ~30,
- `Connection.opener` (a `StreamOpener`, which holds its own `Arc` clone),
- every `StreamOpener` handed to the h3 driver via
  `fn opener(&self) -> StreamOpener { StreamOpener::new(self.conn.clone()) }`
  ([lib.rs](../msquic-h3/src/lib.rs) line ~240).

`ConnectionClose` runs in the msquic crate's `Drop` on the inner handle, which
only fires when the **last** `Arc` clone drops. The h3 driver routinely holds an
`OpenStreams` (a `StreamOpener` clone) that outlives the `Connection` wrapper.
So decrementing in `Drop for Connection` would signal "idle" while the handle is
still open and the rundown ref still held — reintroducing exactly the straggler
race the feature is meant to remove.

**Rule: the decrement must be bound to the drop of the last `Arc` clone of the
handle — the real `ConnectionClose` — not to any user-facing wrapper.**

## Data structures

### Shared rundown state

Lives on the registration, cloned into every tracked handle (connections and
listeners). Executor-agnostic and multi-waiter: an `AtomicUsize` plus a small
`Mutex`-guarded waker table. Only `std` + `futures` types; no new dependency.

```rust
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Waker;

#[derive(Debug, Default)]
struct Waiters {
    next: u64,
    map: HashMap<u64, Waker>,
}

#[derive(Debug, Default)]
struct RundownState {
    /// Outstanding handle reservations plus live tracked handles
    /// (connections + listeners). Reserved eagerly — the client path increments
    /// this when `connect` is called, before `Connection::open` acquires any
    /// native rundown ref — so it conservatively over-counts relative to the
    /// native `Registration->Rundown`. Decremented when the handle's Close runs
    /// (or when a reservation is abandoned without opening).
    active: AtomicUsize,
    /// Wakers for all outstanding `wait_idle` futures. Drained and woken when
    /// `active` transitions to 0. Supports any number of concurrent waiters.
    waiters: Mutex<Waiters>,
}
```

Configurations are deliberately **not** counted here — see [Scope](#scope-connections-and-listeners-tracked-configurations-are-manual).

### Registration wrapper

msquic-h3 currently re-exports `msquic::Registration` and borrows it in
`connect`. We replace it with a wrapper that owns the idle state and exposes only
tracked operations (no public access to the raw handle — see
[API compatibility](#api-compatibility)).

```rust
pub struct Registration {
    inner: msquic::Registration,
    state: Arc<RundownState>,
}

impl Registration {
    pub fn new(config: &msquic::RegistrationConfig) -> Result<Self, Status> {
        Ok(Self {
            inner: msquic::Registration::new(config)?,
            state: Arc::new(RundownState::default()),
        })
    }

    /// Queue shutdown to all connections and stop all listeners. Non-blocking.
    /// Note: `msquic::Registration::shutdown` takes no arguments in 2.5.1-beta.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }

    /// Open a configuration against this registration. Untracked by design
    /// (see Scope): the returned `Configuration` must be dropped before the
    /// `Registration`, after `wait_idle()` has resolved.
    pub fn open_configuration(
        &self,
        alpn: &[msquic::BufferRef],
        settings: Option<&msquic::Settings>,
    ) -> Result<msquic::Configuration, Status> {
        msquic::Configuration::open(&self.inner, alpn, settings)
    }

    /// Resolves once the registration rundown holds no msquic-h3 connection or
    /// listener. Call after `shutdown()` (and after dropping any `Listener`),
    /// before dropping configurations and the registration.
    pub fn wait_idle(&self) -> WaitIdle {
        WaitIdle { state: self.state.clone(), key: None }
    }

    /// Raw handle for crate-internal constructors only.
    pub(crate) fn raw(&self) -> &msquic::Registration {
        &self.inner
    }

    pub(crate) fn state(&self) -> &Arc<RundownState> {
        &self.state
    }
}
```

### Handle newtype carrying the guard

Replace bare `Arc<msquic::Connection>` with `Arc<ConnHandle>`. Field order is
load-bearing:

```rust
#[derive(Debug)] // Connection/StreamOpener derive Debug, so ConnHandle must too
struct ConnHandle {
    inner: msquic::Connection, // dropped FIRST  -> ConnectionClose, releases rundown ref
    _guard: RundownGuard,      // dropped SECOND -> decrement + wake wait_idle
}

impl std::ops::Deref for ConnHandle {
    type Target = msquic::Connection;
    fn deref(&self) -> &msquic::Connection { &self.inner }
}
```

Rust drops struct fields in declaration order, so `inner`'s `Drop`
(`ConnectionClose`, which synchronously releases the registration rundown ref)
completes **before** `_guard` decrements. Therefore, when `wait_idle` observes
`active == 0`, every `ConnectionClose` has already returned and
`RegistrationClose` cannot block.

Because `ConnHandle` derefs to `msquic::Connection`, existing call sites
(`conn.shutdown(...)`, `H3Stream::open_and_start(conn, ...)`, etc.) need no
changes beyond the type alias. `Connection` and `StreamOpener` derive `Debug`, so
`ConnHandle`, `RundownGuard`, and `RundownState` must too — all shown types do.

### The guard

```rust
#[derive(Debug)]
struct RundownGuard {
    state: Arc<RundownState>,
}

impl RundownGuard {
    fn new(state: Arc<RundownState>) -> Self {
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
                let mut w = self.state.waiters.lock().unwrap();
                w.map.drain().map(|(_, waker)| waker).collect()
            };
            for waker in woken {
                waker.wake();
            }
        }
    }
}
```

The same guard type is used for both connections (inside `ConnHandle`) and
listeners (inside the `Listener` wrapper); "the handle field declared before this
guard" is `ConnectionClose` or `ListenerClose` respectively.

The guard is created exactly once per handle and is *not* cloned — for a
connection, only the surrounding `Arc<ConnHandle>` is cloned, so the count
reflects live handles regardless of how many `StreamOpener` clones exist. See
[Wiring the increment points](#wiring-the-increment-points) for *when* the guard
is created — for connections it must be reserved before the future is polled (or
before `attach` on the server), not deferred to handle construction.

### The future

Multi-waiter: each `WaitIdle` owns a slot in the waker table so concurrent
awaiters can't clobber each other.

```rust
pub struct WaitIdle {
    state: Arc<RundownState>,
    key: Option<u64>, // slot in RundownState::waiters, allocated on first Pending
}

impl WaitIdle {
    fn deregister(&mut self) {
        if let Some(k) = self.key.take() {
            self.state.waiters.lock().unwrap().map.remove(&k);
        }
    }
}

impl std::future::Future for WaitIdle {
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
            let mut w = this.state.waiters.lock().unwrap();
            match this.key {
                Some(k) => { w.map.insert(k, cx.waker().clone()); }
                None => {
                    let k = w.next;
                    w.next += 1;
                    w.map.insert(k, cx.waker().clone());
                    this.key = Some(k);
                }
            }
        }
        // Re-check AFTER registering to avoid a lost wakeup against a guard that
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
```

If no handle was ever opened (or all are already closed), the first poll sees `0`
and resolves immediately. A dropped-before-completion `WaitIdle` removes its slot
so the table doesn't leak wakers.

## Wiring the increment points

### Client: reserve before the future is polled

The guard must be reserved **when `connect` is called**, not on first poll and
not at `ConnHandle` construction. Because an `async fn` runs its body only when
first polled, `connect` is written as a **non-async** function that reserves the
guard eagerly and returns an `async move` future that owns it:

```rust
pub fn connect<'a>(
    reg: &'a Registration,
    config: &'a Configuration,
    server_name: &'a str,
    server_port: u16,
) -> impl std::future::Future<Output = Result<Connection, Status>> + 'a {
    // Reserved now, before the caller ever polls. Counts even a queued/unpolled
    // future; if that future is dropped without completing, the guard drops and
    // decrements.
    let guard = RundownGuard::new(reg.state().clone());
    async move {
        // Build the ordered handle immediately after `open`, before `start` and
        // before the first await, so every success/error/cancellation path uses
        // the same explicit ConnectionClose-then-guard drop order proven by
        // ConnHandle's field layout. (Do not leave `inner` and `guard` as
        // separate locals across the await — the generated future's field drop
        // order is unspecified.)
        let inner = msquic::Connection::open(reg.raw(), handler)?;
        let conn = Arc::new(ConnHandle { inner, _guard: guard });
        conn.start(config, server_name, server_port)?; // via Deref to msquic::Connection
        // ... await Connected while `conn` owns both values; on any early return
        // or cancellation, dropping `conn` runs ConnectionClose then decrements ...
        Ok(Connection { conn, /* ctx, opener */ })
    }
}
```

Why non-async matters: msquic acquires the rundown ref in `QuicConnRegister`
(connection.c:501) the moment the native handle is opened. If reservation
happened inside an `async fn` body, a connection future created before teardown
but polled after `wait_idle()` observed `0` could open a native connection
*after* the wait resolved — the exact deadlock. Reserving in the synchronous
prologue makes the future itself carry the count from the instant it exists.

Why build `ConnHandle` before `start`/`await`: once the native handle is open,
both it and the guard live inside a single `Arc<ConnHandle>`, so cancelling the
future while `Connected` is pending drops them in the proven order
(`ConnectionClose` first, decrement second) rather than relying on the async
future's unspecified capture layout.

### Server: reserve at `NewConnection`, close-before-decrement on failure

An accepted connection is registered with the registration (holding the rundown)
*before* `ListenerEvent::NewConnection` is delivered, and the current callback
runs the fallible `set_configuration` before `attach`
([listener.rs](../msquic-h3/src/listener.rs) lines ~52-59). So the guard must be
reserved at the top of `NewConnection` handling, and on the error path the native
handle must be **closed before** the guard decrements:

```rust
ListenerEvent::NewConnection { connection, .. } => {
    let inner = unsafe { msquic::Connection::from_raw(connection.as_raw()) };
    // The native handle already holds a rundown ref here.
    let guard = RundownGuard::new(state.clone());
    if let Err(e) = inner.set_configuration(config) {
        // Close the handle first (releases rundown), THEN drop the guard.
        drop(ConnHandle { inner, _guard: guard });
        return Err(e);
    }
    let conn = crate::Connection::attach(inner, guard); // moves guard into ConnHandle
    // ... send conn to acceptor ...
}
```

`attach` gains a `RundownGuard` parameter and stores it in the `ConnHandle` it
builds. No new native connections are delivered once the listener has stopped
(below), so reserving here plus the listener's own guard closes the
callback/teardown overlap window.

### Server: the listener holds a guard too

A `Listener` opened via `QuicListenerOpen` holds the same rundown
(listener.c:81), released only by `ListenerClose` at drop. So the `Listener`
wrapper carries a guard, dropped after its inner handle:

```rust
pub struct Listener {
    inner: msquic::Listener,  // dropped FIRST -> ListenerClose, releases rundown
    conn: ListenerCtxReceiver,
    _guard: RundownGuard,     // dropped LAST  -> decrement + wake
}

impl Listener {
    pub fn new(
        reg: &Registration,
        config: Arc<msquic::Configuration>,
        alpn: &[msquic::BufferRef],
        local_addr: Option<SocketAddr>,
    ) -> Result<Self, Status> {
        let guard = RundownGuard::new(reg.state().clone());
        // handler captures reg.state() so accepted connections reserve guards
        let inner = msquic::Listener::open(reg.raw(), /* handler */)?;
        inner.start(alpn, /* addr */)?;
        Ok(Self { inner, conn: rx, _guard: guard })
    }
}
```

Because the listener holds a guard, `wait_idle()` **cannot resolve while a
listener is alive** — the "drop the listener before waiting" ordering is enforced
structurally, not by convention.

### Invariant: no new handles after teardown begins

`wait_idle` guarantees that, once it resolves, every connection and listener
*created so far* has released the rundown. It does **not** stop new ones from
being created. The caller must stop creating handles — connections, listeners,
*and* configurations (the last are untracked, so nothing catches a late one) —
before teardown: stop issuing requests / accept loops, `shutdown()`, drop
listeners, then `wait_idle().await`. Eager client reservation means even a
created-but-unpolled `connect` future is counted and will either complete or
drop — so the invariant is about not *creating* new handles, not about polling
state.

## Teardown sequence

### Client

```rust
let reg = msquic_h3::Registration::new(&reg_cfg)?;
let config = reg.open_configuration(&[alpn], Some(&settings))?; // opened against reg
// ... open connections via Connection::connect(&reg, &config, ...), run requests ...
reg.shutdown();          // no args in 2.5.1-beta; queues conn shutdown, non-blocking
reg.wait_idle().await;   // resolves after every connection's ConnectionClose ran
drop(config);            // ConfigurationClose — MUST precede RegistrationClose
drop(reg);               // RegistrationClose returns without blocking
```

### Server

```rust
let reg = msquic_h3::Registration::new(&reg_cfg)?;
let config = Arc::new(reg.open_configuration(&[alpn], Some(&settings))?);
let listener = msquic_h3::Listener::new(&reg, config.clone(), &[alpn], addr)?;
// ... accept loop producing Connections ...

listener.shutdown().await; // stop accepting, drain StopComplete (no more NewConnection)
drop(listener);            // ListenerClose releases the listener's rundown ref
reg.shutdown();            // shut down accepted connections
reg.wait_idle().await;     // resolves once all connections have closed
drop(config);              // after connections, before reg
drop(reg);                 // non-blocking
```

The ordering is partly self-enforcing: the listener holds a rundown guard, so
`wait_idle()` would stay `Pending` if you forgot to drop the listener.
Reconnects are handled for free — each new `ConnHandle` increments the same
counter, so `wait_idle` cannot resolve until that connection also closes.

### Scope: connections and listeners tracked, configurations are manual

`wait_idle` tracks the two rundown holders whose lifetimes msquic-h3 owns and
whose closes are otherwise easy to race: **connections** and **listeners**. It
deliberately does **not** track **configurations**, for a native-lifetime reason.
Assigning a configuration to a connection calls `QuicConfigurationAddRef`
(connection.c:2457); the connection releases it during cleanup
(connection.c:372). `ConfigurationClose` only drops the *application's* ref
(configuration.c:285), and the rundown ref is released inside
`QuicConfigurationRelease` (configuration.c:260) only when the refcount reaches
zero. So `ConfigurationClose` (which is what a wrapper `Drop` would call) can
return while the native configuration is still alive — held by a connection — and
still holding the rundown. A wrapper guard decremented at `ConfigurationClose`
would therefore report idle *too early*.

The correct order is unchanged: wait for all connections to close, then close
the application configuration, then close the registration. Because a connection
releases its configuration ref during its own cleanup, once `wait_idle()`
resolves (all connections closed) the app's `ConfigurationClose` is the last
remaining ref and drops the configuration for real. So the contract keeps one
manual step: **drop configurations after `wait_idle()` resolves and before
`drop(reg)`.** `wait_idle` makes `drop(reg)` non-blocking with respect to
connections and listeners; the caller closes configurations in the shown order.

## Why this is strictly better than `H3MsQuicClientWaiter`

| | app-side waiter | `wait_idle()` |
|---|---|---|
| Signal source | `SHUTDOWN_COMPLETE` event | last `Arc<ConnHandle>` drop (`ConnectionClose`) |
| Timing vs handle close | fires *before* close | fires *after* close |
| Straggler race | possible (handle may still be open) | none |
| Executor | tokio (`watch`, `spawn`) | `futures` only |
| Extra tasks | one spawned task per connection | none |

The app waiter keyed off `SHUTDOWN_COMPLETE` is inherently early: the oneshot in
`connection_callback`'s `ShutdownComplete` arm ([lib.rs](../msquic-h3/src/lib.rs)
line ~98) fires while the `Arc` handle is still alive. `wait_idle` keys off the
actual close, eliminating the race rather than narrowing it.

## Memory-ordering summary

- Increment: `Relaxed` — no ordering needed on the way up.
- Decrement: `Release`, with an `Acquire` fence on the `1 -> 0` edge before
  waking. This publishes the effects of the just-completed `ConnectionClose` /
  `ListenerClose`.
- `WaitIdle::poll` load: `Acquire` — pairs with the decrement `Release`, so a
  reader observing `0` also observes every prior handle close.
- The `waiters` `Mutex` synchronizes waker registration against the wake-all in
  guard `Drop`; the post-registration re-check of `active` (Acquire) closes the
  lost-wakeup window between the fast-path load and taking the lock.

## Limitations and notes

- **Multiple waiters supported.** Each `WaitIdle` owns a slot in the waker table,
  so any number of concurrent awaiters are woken on the `-> 0` edge; none can
  clobber another. A dropped waiter removes its slot.
- **Configurations are manual.** `wait_idle` does not track configurations; drop
  them after it resolves and before the registration (see Scope).
- **Re-arming.** After reaching `0`, creating a new connection or listener
  increments again and a fresh `wait_idle` will block. It is meant to be awaited
  once, after `shutdown()` and after listeners are dropped, when no further
  handles are created.
- **Lock cost.** The `Mutex<Waiters>` is taken only on `wait_idle` polls and on
  the single `-> 0` transition, not on the hot connect/close path (which touches
  only the `AtomicUsize`). Fine for a teardown-path primitive.
- **No new runtime dependency.** Only `std` (`Mutex`, `HashMap`, atomics) and the
  existing `futures`/`Waker` are used; tokio stays a dev-dependency.

## Impact on downstream (`h3-util`)

`H3MsQuicClientWaiter` and its per-connection `tokio::spawn` decrement tasks in
`tonic-h3/h3-util/src/msquic/client.rs` can be deleted. `H3MsQuicConnector` holds
an `Arc<msquic_h3::Registration>`; teardown becomes `reg.shutdown()` →
`reg.wait_idle().await` → `drop`. `get_shutdown_waiter` /
`ConnectionShutdownWaiter` may remain for callers that want the earlier
`SHUTDOWN_COMPLETE` signal, but are no longer required for safe registration
teardown.

## Testing

- **Reconnect race:** open a connection, force the h3 driver to rebuild it
  (drop/reconnect) so an old `StreamOpener` clone briefly coexists with a new
  connection; assert `wait_idle` resolves only after *both* handles are dropped.
- **Opener outlives Connection:** drop the `Connection` wrapper while holding a
  `StreamOpener` from `opener()`; assert `wait_idle` stays `Pending` until the
  opener is also dropped (this is the exact case the old `Drop for Connection`
  proposal got wrong).
- **Idle-from-start:** `wait_idle` on a registration with no handles resolves
  immediately.
- **Listener keeps it pending:** with a stopped-but-not-dropped `Listener` and no
  connections, assert `wait_idle` is `Pending`; assert it resolves after the
  listener is dropped. Guards against the listener-rundown hang.
- **Unpolled connect future counted:** call `connect(...)` but do not poll the
  returned future; assert `wait_idle` is `Pending`, then drop the future and
  assert it resolves. Proves eager reservation.
- **Cancel during `Connected`:** poll a `connect` future far enough to open and
  `start` the native handle, then drop it while `Connected` is still pending;
  assert `wait_idle` resolves only after `ConnectionClose` has run. Covers the
  establishment-cancellation drop path that the unpolled-future test does not.
- **`set_configuration` failure:** simulate an accept whose `set_configuration`
  fails; assert the reserved guard is released (rundown returns to empty) and no
  count leaks.
- **Concurrent waiters:** create two `WaitIdle` futures on the same registration,
  poll both to `Pending`, then drop the last handle; assert *both* complete.
- **Waiter cancellation:** poll a `WaitIdle` to `Pending`, drop it, and assert
  its slot is removed from the waker table before the last guard is released —
  covering the `Drop::deregister` path that prevents stale wakers accumulating.
- **No-hang teardown:** loop many connect + shutdown cycles (and accept +
  listener-stop cycles) under a timeout to guard against `RegistrationClose`
  blocking — the intermittent hang the app waiter had.
- **Public-API doctest:** a doctest exercising `new` → `open_configuration` →
  `connect` → `shutdown` → `wait_idle` → `drop` keeps the documented signatures
  compiling against the pinned `msquic`.
```