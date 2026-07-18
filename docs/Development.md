# Development

## Canonical build / test / bench invocations

Native-library provenance is selected by a **committed, mutually-exclusive**
feature (`native-find` or `native-src`), so `--all-features` is intentionally
invalid (it would enable both and the `msquic` build script rejects that). Verify
with a single provenance row:

```sh
# Canonical local row: system-package libmsquic (Linux 2.5.8 host).
cargo check  --all-targets --no-default-features --features native-find
cargo clippy --all-targets --no-default-features --features native-find -- -D warnings
cargo fmt --all -- --check
cargo test               --no-default-features --features native-find -- --nocapture

# Vendored-source row: crate-built from the bundled msquic source via cmake.
cargo test --no-default-features --features native-src -- --nocapture

# Default / tracing rows (no native tests):
cargo test --all -- --nocapture
cargo test --all --features tracing -- --nocapture

# Send-copy benchmark (separate Criterion crate; needs the gated entry point):
cargo bench --no-default-features --features native-find,bench-internals --bench send_copy
```

If the linked `libmsquic` is not on the default loader path, point the loader at
it (e.g. a locally built copy under `build/`):

```sh
# This is required to run tests when the lib is not already installed system-wide
export LD_LIBRARY_PATH="$(pwd)/build"
```

## Supported configuration matrix

The native-version attestation gate is **per row** (see below). `native-find`
links the system package; `native-src` builds the vendored `msquic-2.5.1-beta`
source.

| OS / arch          | Provenance    | Native `libmsquic` | Expected version | Status                 |
|--------------------|---------------|--------------------|------------------|------------------------|
| Linux x86-64       | `native-find` | system package     | `2.5.8`          | CI + canonical local — attested (SC-012) |
| Linux x86-64       | `native-src`  | crate-built (cmake)| `2.5.1`          | CI — attested (SC-012)  |
| Windows x86-64     | vcpkg / pwsh  | vcpkg-provided     | UNATTESTED       | compile / test only — outside SC-012 |

`MSQUIC_EXPECTED_VERSION="major.minor.patch"` overrides the per-row default so any
supported matrix cell can pin its own value.

> **Windows scope (honest limitation).** The Windows job builds and runs the test
> suite but does **not** run `native_version_preflight`: the loaded-module (DLL)
> path/digest query is not yet implemented, so no version/git-hash/digest is
> attested there. Windows is therefore explicitly **outside** the SC-012 attested
> conformance-support matrix — it has no expected-version pin and must not be
> described as attested until the loaded-module query lands. Only the two Linux
> provenance rows are attested release gates.

## Native-version attestation policy

`native_version_preflight` (`msquic-h3/src/attest.rs`, `#[cfg(test)]`) is a
**blocking** release gate that attests the library the test process *actually
loaded*, not merely an installed package:

1. **Live-handle identity (primary).** Queries `QUIC_PARAM_GLOBAL_LIBRARY_VERSION`
   (`[major, minor, patch, build]`; build id ignored) and
   `QUIC_PARAM_GLOBAL_LIBRARY_GIT_HASH` through the live MsQuic handle. The
   version must equal the **row-specific** pin: `native-find → [2,5,8]`,
   `native-src → [2,5,1]`.
2. **Loaded-artifact digest (secondary, Linux — blocking).** Resolves the
   `libmsquic.so*` the loader actually mapped (via `/proc/self/maps`) and records
   its SHA-256. If the path/digest cannot be resolved the gate **fails** rather
   than passing silently. Platforms without the digest mechanism (e.g. Windows)
   are an explicit, separately-scoped skip.

Every job records the captured version + git hash + loaded-artifact digest as its
attestation.

## Send-copy performance and memory measurements (Phase 9)

Outgoing sends are materialized into a single owned `SendBuffer` via one
`copy_to_bytes` allocation on the production copy path
(`copy_into_send_buffer`). The measurements below feed the **provisional** 16 MiB
`MAX_ADAPTER_SEND` ceiling (FR-012); the ceiling is recorded, not ratified, from
this evidence alone.

| Measurement                         | Result (informational)                          |
|-------------------------------------|-------------------------------------------------|
| Per-send copy cost (16 MiB)         | ~3 ms per owned copy                             |
| Synchronous copy latency vs. size   | 1 KiB ~36 ns … 16 MiB ~3 ms                      |
| Aggregate retained (256 × 16 MiB)   | ~4.29 GB held simultaneously — **unbounded** in the number of in-flight sends (no adapter-level ceiling on the sum) |
| Peak resident per send (16 MiB)     | ≤ `2 × payload + 1 MiB` (blocking `send_copy_peak_resident_bound` test) |

The Criterion timings are informational / relative to a same-run baseline (no hard
wall-time assertion until a pinned, isolated runner exists). The **blocking**
deterministic memory gate is the in-crate `send_copy_peak_resident_bound` test,
which bounds the transient source+destination doubling. The aggregate figure
demonstrates the design's documented unbounded-aggregate concern: retained memory
grows linearly with concurrent outstanding sends.

## Notes and prerequisites

```ps1
$env:RUST_LOG = "info"
cargo test -p c2 -- --nocapture
cargo test -p msquic -- --nocapture
```

One can install msquic from apt:
```
sudo apt-get install libmsquic
```

## use submodules
```sh
git submodule update --init
# init only 1 level
cd submodules/msquic
# for all
git submodule update --init

# for windows
git submodule update --init submodules/xdp-for-windows
```

## Install msquic in vcpkg
```
vcpkg install msquic
```