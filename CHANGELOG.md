# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Cargo pre-1.0 semantics](https://doc.rust-lang.org/cargo/reference/semver.html)
where every `0.0.z` bump may contain breaking changes.

## [Unreleased]

Post-`0.0.7` crate-review fixes. No public API break: the adapter surface is
unchanged and these are behavioral/robustness corrections plus test and
packaging hardening.

### Fixed

- **Faithful terminal-cause reporting (commit-on-delivery).** The shared
  connection terminal slot is now frozen only when a cause is actually delivered
  to a caller through an h3-facing poll (`send_data`/`poll_ready`/`poll_finish`/
  `poll_open`/`accept`); an empty slot is never frozen on a non-delivery, so a
  later, more-specific cause can still be recorded and delivered. `reset()` and a
  successful graceful finish deliver no observable terminal and no longer freeze
  the slot, and the accept path uses the same guarded commit primitive (MF-1 /
  MF-2 / SF-C).
- **Per-stream receive flow-control backpressure.** Receive buffering is now
  bounded per stream: at/over `MAX_RECV_BUFFER` (1 MiB per stream) the callback
  returns `QUIC_STATUS_PENDING` and the stream is re-armed (with its full saved
  indication length) only after the reader drains below the bound, replacing the
  prior eager-ack model. Connection memory is bounded by
  (`MAX_RECV_BUFFER` + one in-flight indication) × the negotiated max concurrent
  streams (SF-A).
- **FFI callback panic backstop.** The connection, stream, and listener adapter
  callback bodies now run under a `catch_unwind` guard that force-closes the
  affected stream/connection handle (stream `ABORT` / connection `NONE`,
  `H3_INTERNAL_ERROR` `0x0102`), performs ownership-aware close-or-reject recovery
  for listeners, wakes terminal waiters, and poisons the context so later events
  short-circuit — containing a panic instead of unwinding across the FFI
  boundary. This is defense-in-depth; peer-reachable paths remain panic-free
  (SF-E).

### Changed

- **Hermetic default test suite.** The external `client_test_apache` smoke test
  (which reaches the public internet) is now `#[ignore]`d so the default suite is
  self-contained; run it explicitly with `--ignored` (MF-3).

### Added

- **docs.rs metadata and provenance guard.** `[package.metadata.docs.rs]` pins
  the `native-src` provenance and `--cfg docsrs` for a self-contained doc build;
  a crate-level `compile_error!` now rejects the neither-provenance
  misconfiguration with an actionable message (the both-enabled case remains an
  upstream build-script panic). `--all-features` is not a valid build/test gate
  (SF-L).

## [0.0.7] - 2026-07-18

The MsQuic → h3 error-propagation refactor. Callbacks are now panic-free across
the FFI boundary, terminals carry their true cause, the send half owns its buffer
with exactly-once reclamation, and the release process is gated on native-version
attestation plus send-copy measurements.

### Added

- Per-operation send-size ceiling: `send_data` payloads above `MAX_ADAPTER_SEND`
  (16 MiB, provisional) are rejected with `StreamErrorIncoming::Unknown(OversizedSend)`
  instead of being submitted.
- Observable peer `STOP_SENDING`: it now surfaces from the send side as
  `StreamErrorIncoming::StreamTerminated { error_code }`, both with a send in
  flight and with none.
- Distinct connection-close causes: peer application close, idle/handshake
  timeout, transport shutdown, and local application close now each map to their
  own `ConnectionErrorIncoming` value.
- Local `SendStream::reset(code)` surfaces as a distinct local reset rather than a
  peer termination; all outgoing application/error codes are clamped to the 62-bit
  QUIC varint maximum before crossing the FFI boundary.
- Runtime native-version attestation gate and a committed provenance feature
  matrix (`native-find` / `native-src`), plus a buildable send-copy benchmark and
  a blocking peak-resident memory test.

### Changed

- **BREAKING — panic-free FFI callbacks.** Connection, stream, and listener
  callbacks no longer `unwrap`/`expect`; a dropped receiver or poisoned lock is
  handled as a recoverable fault instead of unwinding across the FFI boundary.
- **BREAKING — terminal/error classifications.** A peer `RESET_STREAM` now maps to
  `StreamErrorIncoming::StreamTerminated` (previously conflated with EOF); a real
  connection close maps to its specific cause instead of a blanket
  `ApplicationClose { error_code: 0 }`; an empty, non-FIN receive notification is
  no longer treated as end-of-stream (only a FIN flag or peer send-shutdown ends
  the stream); after a local `stop_sending`, subsequent reads return a clean
  end-of-stream rather than a stream error.

### Removed

- **BREAKING — `pub H3Buff` removed.** Outgoing sends now use an internal, owned
  `SendBuffer`; the public `H3Buff` type is gone.

### Migration

- **`H3Buff` consumers:** stop naming `H3Buff`. The adapter now materializes
  outgoing payloads into an internal owned `SendBuffer`; there is no public
  replacement type to name — pass your data through the normal
  `h3::quic::SendStream` API and the adapter copies it into an owned buffer.
- **Error-handling consumers:** update any logic that relied on the old behavior:
  - treating a peer reset as EOF → now a `StreamTerminated` error at the read
    point;
  - matching `ApplicationClose { error_code: 0 }` as a catch-all for connection
    close → now match the specific `ConnectionErrorIncoming` cause (application
    close, `Timeout`, transport `Undefined`, or local close);
  - treating an empty receive as EOF → the empty non-FIN case no longer ends the
    stream;
  - expecting a stream error after your own `stop_sending` → reads now return a
    clean end-of-stream (`Ok(None)`).
- **Oversized sends:** a single `send_data` above 16 MiB (`MAX_ADAPTER_SEND`) is
  now rejected with `OversizedSend`; chunk large payloads below the ceiling.

### Rollback

- Reverting the crate to `0.0.6` restores the prior behavior and public API
  (including `pub H3Buff`, reset-as-EOF, and the blanket `ApplicationClose { 0 }`
  connection mapping). If a consumer cannot yet adopt the new error model, pin
  `msquic-h3 = "0.0.6"` and the last-known-good native/config matrix row.

[Unreleased]: https://github.com/youyuanwu/msquic-h3/compare/v0.0.7...HEAD
[0.0.7]: https://github.com/youyuanwu/msquic-h3/releases/tag/v0.0.7
