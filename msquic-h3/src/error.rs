//! Error vocabulary for the msquic <-> h3 adapter.
//!
//! This module introduces the shared, adapter-owned error types, the scoped
//! terminal enums that record *why* a connection, receive half, or send half
//! ended, and the pure conversion helpers that mint fresh `h3` error values at
//! the polling boundary. It also defines the outgoing application-code clamp.
//!
//! Most of the terminal enums and conversion helpers are wired into the FFI
//! callbacks and polling paths in later phases (Phase 3 onwards); they are
//! introduced here so the vocabulary exists in one place. The `#[allow(dead_code)]`
//! attributes below are scoped to individual not-yet-wired items and each notes
//! the phase that consumes it.

use std::fmt;

use h3::quic::{ConnectionErrorIncoming, StreamErrorIncoming};

use crate::msquic::Status;

/// Maximum value representable by a QUIC 62-bit variable-length integer.
///
/// Outgoing application error codes must never exceed this or msquic rejects the
/// encode. See [`clamp_application_code`].
pub const MAX_QUIC_VARINT: u64 = (1 << 62) - 1;

/// Maximum single `send_data` payload the adapter will accept before rejecting
/// with [`OversizedSend`]. Provisional value (16 MiB); the enforcement site is
/// wired in Phase 6.
#[allow(dead_code)] // referenced by OversizedSend Display; enforced in Phase 6
pub const MAX_ADAPTER_SEND: u64 = 16 * 1024 * 1024;

/// Clamp an outgoing application error code to the QUIC 62-bit varint maximum.
///
/// `(1 << 62) - 1` passes unchanged; `1 << 62` and above clamp down to the
/// maximum. Applied at every site where an application code crosses the FFI
/// boundary (connection close, `stop_sending`, and — in Phase 7 — `reset`).
pub(crate) fn clamp_application_code(code: u64) -> u64 {
    code.min(MAX_QUIC_VARINT)
}

// ---------------------------------------------------------------------------
// Adapter-owned error types.
//
// Each is `Debug + Display + Error + Send + Sync` so it can be `Arc`'d into
// `ConnectionErrorIncoming::Undefined(Arc<dyn Error + Send + Sync>)` or boxed
// into `StreamErrorIncoming::Unknown(Box<dyn Error + Send + Sync>)`.
// ---------------------------------------------------------------------------

/// A `send_data` payload above [`MAX_ADAPTER_SEND`]. One-shot; never stored in
/// the shared send-terminal slot. Constructed at the send-rejection site in
/// Phase 6.
#[derive(Debug)]
#[allow(dead_code)] // constructed at the send-rejection site in Phase 6
pub struct OversizedSend {
    /// Requested payload length in bytes (the `remaining()` that was rejected).
    pub len: usize,
}

impl fmt::Display for OversizedSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "send_data payload of {} bytes exceeds MAX_ADAPTER_SEND ({} bytes)",
            self.len, MAX_ADAPTER_SEND
        )
    }
}

impl std::error::Error for OversizedSend {}

/// A non-application transport shutdown. Both the QUIC status and the wire
/// transport error code are retained for diagnostics; the transport code is a
/// QUIC transport code, not an HTTP/3 application code.
#[derive(Debug)]
pub struct MsQuicTransportError {
    /// The QUIC status that accompanied the transport shutdown.
    pub status: Status,
    /// The wire transport error code (diagnostic only).
    pub error_code: u64,
}

impl fmt::Display for MsQuicTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QUIC transport shutdown: status {} (transport error_code {})",
            self.status, self.error_code
        )
    }
}

impl std::error::Error for MsQuicTransportError {}

/// A local (application-initiated) connection close. Carries no peer code: the
/// required mapping is explicit that we must not invent one.
#[derive(Debug)]
pub struct LocalConnectionClose;

impl fmt::Display for LocalConnectionClose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connection closed locally by the application")
    }
}

impl std::error::Error for LocalConnectionClose {}

/// A local reset via `SendStream::reset(code)`. Distinct from a peer
/// `StreamTerminated`: h3 should not poll a reset send stream, and if it does we
/// surface this rather than pretending the peer terminated the stream.
#[derive(Debug)]
#[allow(dead_code)] // constructed by convert_send / reset() in Phase 7
pub struct LocalStreamReset {
    /// The (already-clamped) local reset code.
    pub code: u64,
}

impl fmt::Display for LocalStreamReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "send stream reset locally with code {}", self.code)
    }
}

impl std::error::Error for LocalStreamReset {}

// ---------------------------------------------------------------------------
// Scoped terminal enums.
//
// Small, adapter-owned reasons recorded in callback-facing state. `Clone` where
// multiple consumers need the reason. Converted to fresh h3 error values only at
// the polling boundary via the `convert_*` helpers below.
// ---------------------------------------------------------------------------

/// Why a connection terminated. The first writer wins for the connection scope.
#[derive(Clone, Debug)]
#[allow(dead_code)] // recorded in the connection terminal slot in Phase 3
pub(crate) enum ConnectionTerminal {
    /// Peer closed with an HTTP/3 application error code.
    PeerApplication(u64),
    /// Idle or handshake timeout.
    Timeout,
    /// Non-application transport shutdown.
    Transport {
        /// The QUIC status that accompanied the shutdown.
        status: Status,
        /// The wire transport error code (diagnostic only).
        error_code: u64,
    },
    /// Local application-initiated close.
    LocalClose,
    /// Internal adapter failure.
    Internal(&'static str),
}

/// Why a receive half terminated. The first writer wins for the receive scope.
#[derive(Clone, Debug)]
#[allow(dead_code)] // recorded on the receive path in Phase 4
pub(crate) enum ReceiveTerminal {
    /// Clean end of stream (peer FIN / send-shutdown).
    Fin,
    /// Peer reset the stream with the given code.
    Reset(u64),
    /// The whole connection terminated.
    Connection(ConnectionTerminal),
    /// Internal adapter failure.
    Internal(&'static str),
}

/// Why a send half terminated. The first writer wins for the send scope.
#[derive(Clone, Debug)]
#[allow(dead_code)] // recorded on the send path in Phases 6-7
pub(crate) enum SendTerminal {
    /// Peer sent `STOP_SENDING` with the given code.
    Stopped(u64),
    /// The whole connection terminated.
    Connection(ConnectionTerminal),
    /// Local `reset(code)` (already clamped). Not a peer termination.
    LocalReset(u64),
    /// A native send/start failure carrying its status.
    Failed(Status),
    /// Internal adapter failure.
    Internal(&'static str),
}

// ---------------------------------------------------------------------------
// Conversion helpers.
//
// These are the *only* places that mint h3 error values from adapter terminals.
// Pure functions; wired into the polling boundary in later phases.
// ---------------------------------------------------------------------------

/// Convert a [`ConnectionTerminal`] into the h3 connection error it represents.
#[allow(dead_code)] // wired at the connection polling boundary in Phase 3
pub(crate) fn convert_conn(t: ConnectionTerminal) -> ConnectionErrorIncoming {
    use ConnectionTerminal as C;
    match t {
        C::PeerApplication(code) => ConnectionErrorIncoming::ApplicationClose { error_code: code },
        C::Timeout => ConnectionErrorIncoming::Timeout,
        C::Transport { status, error_code } => {
            ConnectionErrorIncoming::Undefined(std::sync::Arc::new(MsQuicTransportError {
                status,
                error_code,
            }))
        }
        C::LocalClose => {
            ConnectionErrorIncoming::Undefined(std::sync::Arc::new(LocalConnectionClose))
        }
        C::Internal(msg) => ConnectionErrorIncoming::InternalError(msg.to_string()),
    }
}

/// Convert a [`SendTerminal`] into the h3 stream error it represents.
#[allow(dead_code)] // wired at the send polling boundary in Phase 7
pub(crate) fn convert_send(t: SendTerminal) -> StreamErrorIncoming {
    use SendTerminal as S;
    match t {
        S::Stopped(code) => StreamErrorIncoming::StreamTerminated { error_code: code },
        S::Connection(reason) => StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: convert_conn(reason),
        },
        S::LocalReset(code) => StreamErrorIncoming::Unknown(Box::new(LocalStreamReset { code })),
        S::Failed(status) => StreamErrorIncoming::Unknown(Box::new(status)),
        S::Internal(msg) => StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: ConnectionErrorIncoming::InternalError(msg.to_string()),
        },
    }
}

/// Convert a [`ReceiveTerminal`] into the h3 receive outcome it represents.
///
/// A clean FIN maps to `Ok(None)`; every other terminal maps to an error.
#[allow(dead_code)] // wired at the receive polling boundary in Phase 4
pub(crate) fn convert_recv(
    t: ReceiveTerminal,
) -> Result<Option<bytes::Bytes>, StreamErrorIncoming> {
    use ReceiveTerminal as R;
    match t {
        R::Fin => Ok(None),
        R::Reset(code) => Err(StreamErrorIncoming::StreamTerminated { error_code: code }),
        R::Connection(reason) => Err(StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: convert_conn(reason),
        }),
        R::Internal(msg) => Err(StreamErrorIncoming::ConnectionErrorIncoming {
            connection_error: ConnectionErrorIncoming::InternalError(msg.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_application_code_table() {
        // (name, input, expected)
        let cases: &[(&str, u64, u64)] = &[
            ("small value unchanged", 42, 42),
            ("zero unchanged", 0, 0),
            ("max varint unchanged", (1u64 << 62) - 1, (1u64 << 62) - 1),
            ("one over max clamps", 1u64 << 62, (1u64 << 62) - 1),
            ("u64::MAX clamps", u64::MAX, (1u64 << 62) - 1),
        ];
        for (name, input, expected) in cases {
            assert_eq!(clamp_application_code(*input), *expected, "case: {name}");
        }
    }

    #[test]
    fn max_quic_varint_value() {
        assert_eq!(MAX_QUIC_VARINT, (1u64 << 62) - 1);
    }
}

/// Skeleton table-driven harness for the send-side terminal reducer.
///
/// The reducer itself (the send-side state machine that resolves provisional
/// cancellations into concrete [`SendTerminal`] reasons) is implemented in
/// Phase 7. This scaffold establishes the seam so that phase can drop in
/// table-driven cases without restructuring the test module.
#[cfg(test)]
mod reducer_tests {
    /// One row of the reducer table: a human-readable name plus the
    /// (state, event) -> effect triple Phase 7 will populate.
    #[allow(dead_code)] // fields consumed by the reducer table in Phase 7
    struct ReducerCase {
        /// Human-readable case name for assertion messages.
        name: &'static str,
    }

    /// Run one reducer case. Phase 7 replaces this stub body with a call into
    /// the real reducer and an assertion on the produced effect.
    #[allow(dead_code)] // exercised once the reducer exists in Phase 7
    fn run_case(_case: &ReducerCase) {
        // Placeholder: intentionally empty until the reducer lands in Phase 7.
    }

    #[test]
    fn reducer_harness_scaffold_compiles() {
        // Trivial placeholder proving the harness compiles and is wired into the
        // test runner. Phase 7 fills `cases` with real rows and asserts on
        // `run_case` outcomes.
        let cases: &[ReducerCase] = &[ReducerCase {
            name: "placeholder",
        }];
        for case in cases {
            run_case(case);
        }
        assert_eq!(cases.len(), 1);
    }
}
