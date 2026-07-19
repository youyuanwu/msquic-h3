# Testing

The adapter is tested with a mix of pure unit tables, seam-injected doubles, a
real loopback conformance suite, and a native-library attestation gate. This
document explains those layers and the CI matrix that runs them. The canonical
commands are summarized under [Commands](#commands) and detailed in
[Development](./Development.md).

## Layers

- **Pure reducer tables.** The send state machine (`transition()`) takes no native
  handle, so every transition is unit-tested as a table in `error.rs`
  (`reducer_tests`). This is where the send-side ordering and refinement rules are
  proven exhaustively.
- **Seam-injected unit tests.** The callback-driven paths are tested against
  in-process doubles injected through trait seams (below), without a real QUIC
  connection. These live in `lib.rs` test modules (`callback_safety`,
  `connection_terminal`, `receive_events`, `stream_open_identity`, `send_seam`,
  `downcall_clamp`) and in `listener.rs` / `buffer.rs` / `registration.rs`.
- **Loopback conformance.** The `conformance` module's `run_loopback` harness
  drives a real client↔server HTTP/3 exchange over msquic on an ephemeral loopback
  port, exercising accept, send, receive, and panic-free teardown end to end. The
  server accept path also has a focused `basic_server_test` in `listener.rs`.
- **Native attestation.** Before the release conformance suite runs, a gate
  attests the actual libmsquic the process loaded (below).
- **Feature-config negative.** `feature_config_negative` (an `#[ignore]`d test in
  `lib.rs`) asserts that enabling both provenance features fails as expected
  (below).

## Test seams

Several boundaries are expressed as traits so a test can inject a double while
production wires the real implementation:

| Seam | Production impl | Purpose |
| --- | --- | --- |
| `SendExec` | `StreamExecutor` | Submitting owned sends, graceful finish, reset. |
| `RecvExec` | `RecvStreamExecutor` | `stop_sending` and `receive_complete` re-arm. |
| `OpenExec` | `StreamOpenExecutor` | Opening/starting streams. |
| `ShutdownSeam` | `ConnectionRef` / `StreamRef` / `NoShutdown` | Force-close on panic recovery. |

Test constructors (`H3SendStream::with_exec`, `H3RecvStream::with_exec`,
`StreamOpener::with_open_exec`, and the `stream_ctx_channel*` helpers) build a
stream wired to a double. The important property is that the one owned-send
transaction, `submit_owned_send`, is routed by **both** the production
`StreamExecutor` and the test `CountingExec`, so an ownership regression cannot
pass in one and fail in the other.

### Native-test mechanisms

Some guarantees cannot be made deterministic through a real loopback connection,
because the public API cannot force the precise native timing they depend on. The
clearest example is the exactly-once reclamation of the owned `SendBuffer`: a real
loopback send completes synchronously, so a test driving the public API cannot hold
a native send observably outstanding across a drop-triggered `StreamClose`, and
therefore cannot *count* reclamations on the real path.

The adapter handles this by proving such guarantees at the seam instead of at the
wire. The `CountingExec` seam asserts the concrete reclamation count on the
production `SendComplete` path — the count a real loopback send cannot expose —
while the loopback conformance test asserts the observable end-to-end properties
(ordering, completion, and panic-free teardown). Where a guarantee rests on native
behavior that neither layer can make deterministic (for example, msquic's
inline-drain-on-close during `StreamClose`), it is documented as a
source-review-only compatibility expectation on the linked library rather than
asserted by a test that could not be made reliable — see
[callback safety](./callback-safety.md#native-stream-teardown-on-drop).

## Native attestation gate

The `attest` module (test-only) verifies the libmsquic the test process actually
loaded, so a conformance pass cannot be attributed to the wrong native build:

- `library_version` reads `QUIC_PARAM_GLOBAL_LIBRARY_VERSION`
  (`[major, minor, patch, build]`) and `library_git_hash` reads the git hash,
  through the live handle this process opened — both are produced by code inside
  the loaded image.
- On Linux, `loaded_libmsquic_digest` parses `/proc/self/maps` for the mapped
  `libmsquic.so*` and SHA-256s it (via a dependency-free `sha256_hex`). This is a
  blocking part of the gate on Linux: if the loaded digest cannot be resolved the
  attestation fails.

The expected version is **row-specific**: `native-find` links the system package
(CI installs `2.5.8`) so it expects `[2, 5, 8]`, while `native-src` builds the
vendored source and expects `[2, 5, 1]`. `MSQUIC_EXPECTED_VERSION` can override the
expectation. The blocking test `native_version_preflight` asserts major/minor/patch
and a non-empty git hash, and on Linux fails if the loaded digest is unresolved.
Platforms without the digest mechanism (Windows) are an explicit, separately-scoped
skip.

## Commands

The canonical commands (the same gates CI runs — do not invent new ones for the
crate):

```sh
# Type-check the default (neither-provenance) features — no native library needed
cargo check --all-targets
cargo check --all-targets --features tracing

# Format and lint (lint is enforced with -D warnings)
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings

# Test — requires exactly ONE provenance feature
cargo test --no-default-features --features native-find -- --nocapture   # system libmsquic
cargo test --no-default-features --features native-src                   # vendored source build

# Send-copy benchmark (build + run)
cargo bench -p msquic-h3 --no-default-features --features native-find,bench-internals --bench send_copy -- --noplot
```

`--all-features` is **not** valid: it enables both provenance features, which are
mutually exclusive (see below).

## Provenance features and the build matrix

The crate links libmsquic in one of two mutually exclusive ways, selected by
feature:

- **`native-find`** links a preinstalled system libmsquic.
- **`native-src`** builds libmsquic from vendored source via cmake.

These are mutually exclusive: enabling both trips the upstream msquic build
script, so `--all-features` is intentionally never used. The both-enabled case is
proven to fail by the `#[ignore]`d negative test
`both_features_mutually_exclusive_negative`, which asserts the upstream message
`"feature src and find are mutually exclusive"`. This is enforced by the **upstream
build script**, not by a crate-level guard.

The **default** feature set enables *neither* provenance. That is a supported,
link-free configuration: the crate type-checks (`cargo check`) without a native
library, and the "neither" case only fails at link time (a real build/test), never
at `cargo check`. There is deliberately no crate-level `compile_error!` for the
neither case, because CI relies on being able to type-check the default features.

CI (`.github/workflows/build.yaml`) runs three jobs, each across the Rust toolchain
matrix `[1.90.0, 1.97.0]`:

1. **Default compile/lint** — `cargo check`/`clippy --all-targets` with the default
   (neither-provenance) features and again with `--features tracing`, plus
   `cargo fmt --all -- --check`. Lint runs with `-D warnings`. No test/attestation
   here.
2. **Linux conformance** — runs for each provenance (`native-find`, `native-src`)
   with the row-specific expected version, running check/fmt/clippy/test plus a
   **blocking** gate that runs the four attestation + seam/conformance tests
   (`native_version_preflight`, the two `send_seam` reclamation tests, and the
   loopback `accepted_send_completes_and_teardown_is_panic_free`) and a send-copy
   benchmark build-and-run.
3. **Windows** — links libmsquic via vcpkg (`native-find`) and runs the tests with
   `native_version_preflight` skipped (the version is not asserted on Windows).

The default test suite is hermetic: the external `client_test_apache` smoke test is
`#[ignore]`d, and the loopback `conformance` suite covers the client path without a
network dependency.
