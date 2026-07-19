//! Public, validated memory-budget configuration for the adapter.
//!
//! [`H3Config`] carries the per-stream send-size ceiling, the per-stream
//! receive-byte budget, and the per-stream receive-unit (allocation/message)
//! cap. Its fields are **private** and can only be produced via [`Default`] or
//! the validated [`H3ConfigBuilder`], so no invalid configuration is ever
//! constructible by a literal or a field mutation. The defaults equal the
//! adapter's historical hard-coded ceilings ([`MAX_ADAPTER_SEND`] for sends and
//! [`MAX_RECV_BUFFER`] for receive bytes), so a default-configured adapter
//! behaves exactly as before.
//!
//! The send side is bounded by `max_send_bytes × concurrent streams` (there is
//! no aggregate/per-connection send budget — the h3 trait contract calls the
//! synchronous `send_data` before the only async readiness hook, so a send
//! cannot be deferred as backpressure); applications that need to bound send
//! memory should also cap their concurrent stream count.

use std::fmt;

use crate::error::MAX_ADAPTER_SEND;
use crate::stream::MAX_RECV_BUFFER;

/// Default per-stream receive-unit (buffered allocation/message) cap.
///
/// Bounds the number of queued receive indications per stream, independently of
/// their byte total, so a flood of tiny frames cannot amplify into unbounded
/// per-stream allocations. The byte budget alone does not bound this.
pub(crate) const DEFAULT_MAX_RECV_UNITS: usize = 16384;

/// Validated, `Copy` memory-budget configuration threaded from connection /
/// listener construction into every per-connection and per-stream state.
///
/// Fields are private; construct via [`H3Config::default`] or
/// [`H3Config::builder`]. All accessors are read-only, so a value cannot be
/// mutated after construction.
///
/// The receive side is enforced per stream as real backpressure (both a byte
/// budget and a unit-count budget). The **send** side is only bounded by
/// `max_send_bytes` per outstanding send; there is no aggregate send cap, so
/// per-connection send memory scales as `max_send_bytes × concurrent
/// in-flight streams`. The adapter cannot enforce an aggregate send budget as
/// backpressure because the h3 trait contract calls the synchronous
/// `send_data` (which copies and takes ownership) before its only async
/// readiness hook — applications that need to bound send memory should choose
/// `max_send_bytes` and cap their concurrent stream count.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct H3Config {
    /// Maximum single `send_data` payload accepted before rejecting with
    /// `OversizedSend`. Must be non-zero and below `u32::MAX`.
    max_send_bytes: u64,
    /// Maximum undrained, adapter-buffered received bytes per stream before the
    /// receive callback pends. Must be non-zero.
    max_recv_bytes: usize,
    /// Maximum buffered receive units (allocations/messages) per stream. Must be
    /// non-zero.
    max_recv_units: usize,
}

impl Default for H3Config {
    fn default() -> Self {
        Self {
            max_send_bytes: MAX_ADAPTER_SEND,
            max_recv_bytes: MAX_RECV_BUFFER,
            max_recv_units: DEFAULT_MAX_RECV_UNITS,
        }
    }
}

impl H3Config {
    /// Start building a validated [`H3Config`].
    pub fn builder() -> H3ConfigBuilder {
        H3ConfigBuilder::default()
    }

    /// Construct a validated [`H3Config`] directly from its three caps.
    ///
    /// Returns a [`ConfigError`] if any value is out of range (see
    /// [`H3ConfigBuilder::build`]).
    pub fn try_new(
        max_send_bytes: u64,
        max_recv_bytes: usize,
        max_recv_units: usize,
    ) -> Result<Self, ConfigError> {
        validate(max_send_bytes, max_recv_bytes, max_recv_units)
    }

    /// Maximum single `send_data` payload accepted before rejecting with
    /// `OversizedSend`.
    pub fn max_send_bytes(&self) -> u64 {
        self.max_send_bytes
    }

    /// Maximum undrained, adapter-buffered received bytes per stream before the
    /// receive callback pends.
    pub fn max_recv_bytes(&self) -> usize {
        self.max_recv_bytes
    }

    /// Maximum buffered receive units (allocations/messages) per stream.
    pub fn max_recv_units(&self) -> usize {
        self.max_recv_units
    }
}

/// Chainable builder for a validated [`H3Config`].
///
/// Each `with_*` setter overrides one cap; unset caps keep their default. The
/// terminal [`build`](H3ConfigBuilder::build) validates all three and yields a
/// [`ConfigError`] on the first out-of-range value.
#[derive(Copy, Clone, Debug)]
pub struct H3ConfigBuilder {
    max_send_bytes: u64,
    max_recv_bytes: usize,
    max_recv_units: usize,
}

impl Default for H3ConfigBuilder {
    fn default() -> Self {
        let d = H3Config::default();
        Self {
            max_send_bytes: d.max_send_bytes,
            max_recv_bytes: d.max_recv_bytes,
            max_recv_units: d.max_recv_units,
        }
    }
}

impl H3ConfigBuilder {
    /// Override the per-send-size ceiling.
    pub fn with_max_send_bytes(mut self, max_send_bytes: u64) -> Self {
        self.max_send_bytes = max_send_bytes;
        self
    }

    /// Override the per-stream receive-byte budget.
    pub fn with_max_recv_bytes(mut self, max_recv_bytes: usize) -> Self {
        self.max_recv_bytes = max_recv_bytes;
        self
    }

    /// Override the per-stream receive-unit cap.
    pub fn with_max_recv_units(mut self, max_recv_units: usize) -> Self {
        self.max_recv_units = max_recv_units;
        self
    }

    /// Validate the accumulated caps and produce an [`H3Config`].
    ///
    /// Rejects `max_send_bytes == 0` or `>= u32::MAX`
    /// ([`ConfigError::SendSize`]), `max_recv_bytes == 0`
    /// ([`ConfigError::RecvBytes`]), and `max_recv_units == 0`
    /// ([`ConfigError::RecvUnits`]).
    pub fn build(self) -> Result<H3Config, ConfigError> {
        validate(
            self.max_send_bytes,
            self.max_recv_bytes,
            self.max_recv_units,
        )
    }
}

/// Shared validation for [`H3ConfigBuilder::build`] and [`H3Config::try_new`].
fn validate(
    max_send_bytes: u64,
    max_recv_bytes: usize,
    max_recv_units: usize,
) -> Result<H3Config, ConfigError> {
    if max_send_bytes == 0 || max_send_bytes >= u32::MAX as u64 {
        return Err(ConfigError::SendSize);
    }
    if max_recv_bytes == 0 {
        return Err(ConfigError::RecvBytes);
    }
    if max_recv_units == 0 {
        return Err(ConfigError::RecvUnits);
    }
    Ok(H3Config {
        max_send_bytes,
        max_recv_bytes,
        max_recv_units,
    })
}

/// A rejected [`H3Config`] cap value.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// `max_send_bytes` was zero or `>= u32::MAX` (the native send-length
    /// ceiling, above which a `BufferRef` length would truncate).
    SendSize,
    /// `max_recv_bytes` was zero (a stream could never buffer any receive data).
    RecvBytes,
    /// `max_recv_units` was zero (a stream could never buffer any receive unit).
    RecvUnits,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::SendSize => write!(
                f,
                "max_send_bytes must be non-zero and less than u32::MAX ({})",
                u32::MAX
            ),
            ConfigError::RecvBytes => write!(f, "max_recv_bytes must be non-zero"),
            ConfigError::RecvUnits => write!(f, "max_recv_units must be non-zero"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::{ConfigError, DEFAULT_MAX_RECV_UNITS, H3Config};
    use crate::error::MAX_ADAPTER_SEND;
    use crate::stream::MAX_RECV_BUFFER;

    #[test]
    fn defaults_equal_the_historical_constants() {
        let c = H3Config::default();
        assert_eq!(c.max_send_bytes(), MAX_ADAPTER_SEND);
        assert_eq!(c.max_recv_bytes(), MAX_RECV_BUFFER);
        assert_eq!(c.max_recv_units(), DEFAULT_MAX_RECV_UNITS);
        assert_eq!(c.max_recv_units(), 16384);
    }

    #[test]
    fn builder_defaults_match_default() {
        let built = H3Config::builder().build().unwrap();
        let def = H3Config::default();
        assert_eq!(built.max_send_bytes(), def.max_send_bytes());
        assert_eq!(built.max_recv_bytes(), def.max_recv_bytes());
        assert_eq!(built.max_recv_units(), def.max_recv_units());
    }

    #[test]
    fn valid_builder_round_trips_accessors() {
        let c = H3Config::builder()
            .with_max_send_bytes(4096)
            .with_max_recv_bytes(8192)
            .with_max_recv_units(7)
            .build()
            .unwrap();
        assert_eq!(c.max_send_bytes(), 4096);
        assert_eq!(c.max_recv_bytes(), 8192);
        assert_eq!(c.max_recv_units(), 7);
    }

    #[test]
    fn try_new_round_trips() {
        let c = H3Config::try_new(4096, 8192, 7).unwrap();
        assert_eq!(c.max_send_bytes(), 4096);
        assert_eq!(c.max_recv_bytes(), 8192);
        assert_eq!(c.max_recv_units(), 7);
    }

    #[test]
    fn zero_send_size_is_rejected() {
        assert_eq!(
            H3Config::builder().with_max_send_bytes(0).build(),
            Err(ConfigError::SendSize)
        );
    }

    #[test]
    fn send_size_at_or_above_u32_max_is_rejected() {
        assert_eq!(
            H3Config::builder()
                .with_max_send_bytes(u32::MAX as u64)
                .build(),
            Err(ConfigError::SendSize)
        );
        assert_eq!(
            H3Config::builder()
                .with_max_send_bytes(u32::MAX as u64 + 1)
                .build(),
            Err(ConfigError::SendSize)
        );
        // One below u32::MAX is the largest accepted ceiling.
        assert!(
            H3Config::builder()
                .with_max_send_bytes(u32::MAX as u64 - 1)
                .build()
                .is_ok()
        );
    }

    #[test]
    fn zero_recv_bytes_is_rejected() {
        assert_eq!(
            H3Config::builder().with_max_recv_bytes(0).build(),
            Err(ConfigError::RecvBytes)
        );
    }

    #[test]
    fn zero_recv_units_is_rejected() {
        assert_eq!(
            H3Config::builder().with_max_recv_units(0).build(),
            Err(ConfigError::RecvUnits)
        );
    }

    #[test]
    fn config_error_displays_a_message_per_variant() {
        assert!(!ConfigError::SendSize.to_string().is_empty());
        assert!(!ConfigError::RecvBytes.to_string().is_empty());
        assert!(!ConfigError::RecvUnits.to_string().is_empty());
    }
}
