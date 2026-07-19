# Registration lifecycle

`Registration::wait_idle()` lets client and server code tear down a
`Registration` deterministically, using only `futures`/`std` so the adapter stays
executor-agnostic (no tokio dependency). This document explains why the machinery
exists and the teardown order it requires. It is implemented across
[`registration.rs`](../msquic-h3/src/registration.rs),
[`lib.rs`](../msquic-h3/src/lib.rs), and
[`listener.rs`](../msquic-h3/src/listener.rs).

## The constraint

Every msquic object opened against a `Registration` acquires the **same**
`Registration` rundown, and `RegistrationClose` — run from the registration's
`drop` — synchronously blocks until all of them are released. The rundown is held
by connections (released from `ConnectionClose`), listeners (released from
`ListenerClose`), and configurations (released when the native config refcount
reaches zero; a connection holds its own config ref, so a config can outlive
`ConfigurationClose`).

`RegistrationShutdown` only shuts down connections and stops listeners — it closes
nothing. The tracked handles close only when their wrappers (and every `Arc` clone)
drop. So the full safe teardown order, as demonstrated by the loopback conformance
harness, is: drop the connections → drop the listener → `shutdown` → `wait_idle`
(the "wait for tracked handles to close" step) → drop configurations → drop the
registration.

## Core correctness rule

The native connection handle is shared behind an `Arc` (the `Connection` wrapper,
its `StreamOpener`, and every `StreamOpener` clone the h3 driver holds).
`ConnectionClose` runs only when the **last** `Arc` clone drops, which can outlive
the public `Connection`. The rundown decrement must therefore be bound to that
final drop, not to any user-facing wrapper's `Drop`.

This is realized by `ConnHandle`: a newtype wrapping the native handle plus a
`RundownGuard`, with the handle field declared **before** the guard so
`ConnectionClose` runs before the guard decrements and wakes waiters. `Connection`
and `StreamOpener` hold `Arc<ConnHandle>`.

## How rundown tracking works

- **`RundownState`** holds an `AtomicUsize active` count plus a `Mutex`-guarded
  waker table. The hot connect/close path touches only the atomic; the table is
  touched only when a `wait_idle` future parks or wakes.
- **`RundownGuard`** increments the count on `new` (`Relaxed`) and decrements on
  `Drop` (`Release`). On the decrement that reaches zero it issues an `Acquire`
  fence and then drains and wakes every registered waiter, so a waiter that
  observes zero also observes the preceding handle close.
- **`WaitIdle`** is a multi-waiter `Future`: it fast-paths when `active == 0`,
  otherwise registers (or refreshes) its waker in a slot under the lock and
  re-checks to avoid a lost wakeup. Each `WaitIdle` owns its slot and deregisters
  on drop.

## Key design decisions

- **Eager reservation.** `Connection::connect` is a non-async fn that reserves the
  guard synchronously, before the returned future is polled, and builds
  `ConnHandle` immediately after `open`. msquic acquires the rundown at handle
  open, so a late reservation could let `wait_idle` observe zero while a connect is
  still in flight. Eager reservation also puts establishment cancellation on the
  proven close-then-decrement order.
- **Listeners are tracked too.** `Listener` reserves a guard *before* `ListenerOpen`
  and drops it after `ListenerClose`, so `wait_idle` cannot resolve while a listener
  is alive — the drop-listener-first ordering is structural, not by convention.
- **Accepted connections** reserve the guard at the top of `NewConnection` handling,
  before the fallible `set_configuration`, and close-before-decrement on the error
  path.
- **Configurations are not tracked.** A guard at `ConfigurationClose` would report
  idle too early (the native config can persist until connections release their
  refs). Instead the caller drops configurations after `wait_idle` resolves and
  before the registration — the one manual step.

## Encapsulation

The raw `msquic::Registration` is `pub(crate)` only — there is no public `Deref`
or accessor — so callers cannot open untracked connections or listeners, nor reach
the handle whose `drop` is the blocking `RegistrationClose`. All tracked handles
are created through controlled entry points (`Connection::connect`, `Listener::new`,
`Registration::open_configuration`).

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

A server additionally stops and drops the `Listener` before `wait_idle`
(`wait_idle` stays pending until then, since the listener holds a guard). The
loopback conformance harness performs exactly this order: drop connections → drop
listener → `shutdown` → `wait_idle` → drop configurations → drop registration.

`wait_idle` reports on handles created *so far*; it does not prevent new ones.
Callers must stop creating connections, listeners, and (untracked) configurations
before teardown.

## Tests

Unit tests in [`registration.rs`](../msquic-h3/src/registration.rs) cover the
counter/waiter behaviour (idle-from-start, reservation blocks, wake-all, concurrent
waiters, drop-deregister, nested reservations). Native coverage:
`listener_keeps_wait_idle_pending` in [`listener.rs`](../msquic-h3/src/listener.rs)
and the client `shutdown → wait_idle` cycle exercised by the loopback conformance
suite. See [testing](./testing.md) for the overall test strategy.
