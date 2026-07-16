# Design: `Registration::wait_idle()` for msquic-h3

Executor-agnostic registration-idle tracking that lets client and server code
tear down a `Registration` without the app-side `H3MsQuicClientWaiter` shim.

Implemented in [registration.rs](../msquic-h3/src/registration.rs),
[lib.rs](../msquic-h3/src/lib.rs), and [listener.rs](../msquic-h3/src/listener.rs).
Companion to the analysis in `tonic-h3/docs/msquic-registration-shutdown.md`.

## The constraint

Every msquic object opened against a `Registration` acquires the **same**
`Registration->Rundown`, and `RegistrationClose` (run from `drop`) synchronously
blocks on `CxPlatRundownReleaseAndWait` until all are released. In msquic
2.5.1-beta that rundown is held by:

- **Connections** — released from `ConnectionClose` (connection.c:501/395).
- **Listeners** — released from `ListenerClose` (listener.c:81/148).
- **Configurations** — released from `QuicConfigurationRelease` at refcount zero
  (configuration.c:199/260); a connection holds its own config ref, so a config
  can outlive `ConfigurationClose`.

`RegistrationShutdown` only shuts down connections and stops listeners — it
closes nothing. So the safe teardown order is `shutdown` → wait for tracked
handles to close → drop configs → drop registration. `wait_idle` provides the
"wait" step using only `futures`/`std` (no tokio; msquic-h3 stays
executor-agnostic).

## Core correctness rule

The msquic connection handle is shared behind an `Arc` (the `Connection` wrapper,
its `StreamOpener`, and every `StreamOpener` clone the h3 driver holds).
`ConnectionClose` runs only when the **last** `Arc` clone drops — which can
outlive the public `Connection`. So the rundown decrement must be bound to that
final drop, not to any user-facing wrapper's `Drop`.

This is realized by `ConnHandle`: a newtype wrapping the native handle plus a
`RundownGuard`, with the handle field declared **before** the guard so
`ConnectionClose` runs before the guard decrements and wakes waiters.
`Connection` and `StreamOpener` hold `Arc<ConnHandle>`.

## Key design decisions

- **Eager reservation.** `Connection::connect` is a non-async fn that reserves
  the guard synchronously (before the returned future is polled) and builds
  `ConnHandle` immediately after `open`. This counts even queued/unpolled
  connects and puts establishment cancellation on the proven
  close-then-decrement order. msquic acquires the rundown at handle open, so a
  late reservation could let `wait_idle` observe zero while a connect is
  in flight.
- **Listeners are tracked too.** `Listener` holds a guard reserved *before*
  `ListenerOpen` (which acquires the rundown) and dropped after `ListenerClose`.
  Consequently `wait_idle` cannot resolve while a listener is alive — the
  drop-listener-first ordering is structural, not by convention.
- **Accepted connections** reserve the guard at the top of `NewConnection`
  handling, before the fallible `set_configuration`, and close-before-decrement
  on the error path.
- **Configurations are not tracked.** `ConfigurationClose` only drops the app
  ref; the native config (and its rundown ref) can persist until connections
  release their refs, so a guard at `ConfigurationClose` would report idle too
  early. Instead the caller drops configurations after `wait_idle` resolves and
  before the registration — one manual step.
- **Multi-waiter.** `wait_idle` supports any number of concurrent awaiters via a
  `Mutex`-guarded waker table (each `WaitIdle` owns a slot, removed on drop). The
  hot connect/close path touches only the `AtomicUsize`; the guard decrement uses
  `Release` + an `Acquire` fence on the `-> 0` edge so a waiter observing zero
  also observes the preceding handle close.

## API compatibility

Intentionally breaking (0.0.x). `msquic::Registration` is no longer the primary
type; a wrapper owns the idle state and exposes only controlled entry points
(`Connection::connect`, `Listener::new`, `Registration::open_configuration`). The
raw registration is `pub(crate)` only — no public `Deref`/accessor — so callers
cannot open untracked connections or listeners, nor reach the handle whose `drop`
is the blocking `RegistrationClose`.

## Teardown sequence

Client:

```rust
let reg = msquic_h3::Registration::new(&reg_cfg)?;
let config = reg.open_configuration(&[alpn], Some(&settings))?;
// ... Connection::connect(&reg, &config, ...); run requests ...
reg.shutdown();          // non-blocking
reg.wait_idle().await;   // resolves after every ConnectionClose ran
drop(config);            // before the registration
drop(reg);               // no longer blocks
```

Server: additionally stop and `drop` the `Listener` before `wait_idle`
(`wait_idle` stays pending until then, since the listener holds a guard).

`wait_idle` reports on handles *created so far*; it does not prevent new ones.
Callers must stop creating connections, listeners, and (untracked) configurations
before teardown.

## Impact on downstream (`h3-util`)

`H3MsQuicClientWaiter` and its per-connection `tokio::spawn` tasks can be deleted;
teardown becomes `reg.shutdown()` → `reg.wait_idle().await` → `drop`.
`ConnectionShutdownWaiter` may remain for callers wanting the earlier
`SHUTDOWN_COMPLETE` signal.

## Tests

Unit tests in [registration.rs](../msquic-h3/src/registration.rs) cover the
counter/waiter behaviour (idle-from-start, reservation blocks, wake-all,
concurrent waiters, drop-deregister, nested reservations). Native coverage:
`listener_keeps_wait_idle_pending` in [listener.rs](../msquic-h3/src/listener.rs)
and the client `shutdown → wait_idle` cycle in `send_get_request`. Follow-ups:
`StreamOpener`-outlives-`Connection` and cancel-during-connect against a loopback
server.
