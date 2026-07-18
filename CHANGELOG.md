# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Cargo pre-1.0 semantics](https://doc.rust-lang.org/cargo/reference/semver.html)
where every `0.0.z` bump may contain breaking changes.

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

[0.0.7]: https://github.com/youyuanwu/msquic-h3/releases/tag/v0.0.7
