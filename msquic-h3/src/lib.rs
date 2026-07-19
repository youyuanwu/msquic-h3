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
mod config;
pub use config::{ConfigError, H3Config, H3ConfigBuilder};
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
mod test;

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
mod conformance;

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
mod feature_config_negative;
