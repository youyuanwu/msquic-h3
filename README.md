# msquic-h3

[![build](https://github.com/youyuanwu/msquic-h3/actions/workflows/build.yaml/badge.svg)](https://github.com/youyuanwu/msquic-h3/actions/workflows/build.yaml)
[![docs.rs](https://docs.rs/msquic-h3/badge.svg)](https://docs.rs/msquic-h3)
[![crates.io](https://img.shields.io/crates/v/msquic-h3.svg)](https://crates.io/crates/msquic-h3)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

A Rust adapter that lets you run [hyperium's `h3`](https://github.com/hyperium/h3)
HTTP/3 stack on top of [Microsoft's msquic](https://github.com/microsoft/msquic)
QUIC implementation.

`msquic-h3` implements `h3`'s transport traits (`OpenStreams`, `Connection`,
`SendStream`, `RecvStream`, …) over msquic's FFI, so you can drive an HTTP/3
client or server with the standard `h3` API while msquic handles the QUIC
protocol and the socket I/O.

It can also be used as a QUIC transport backend for [tonic-h3](https://github.com/youyuanwu/tonic-h3) to run gRPC over HTTP/3.

> **Status: experimental (`0.0.x`).** The API may change between releases. It can
> run HTTP/3 clients and servers today.

## How it fits together

```
   your app  ──uses──►  h3 (HTTP/3)  ──transport traits──►  msquic-h3  ──FFI──►  msquic (QUIC + UDP)
```

`h3` owns HTTP/3 framing and request/response semantics; msquic owns the QUIC
protocol, TLS, and the network threads. `msquic-h3` is the thin, runtime-agnostic
bridge between them: its core depends only on `futures` (no built-in async
runtime), and it turns msquic's callback/threading model into the poll-based
streams `h3` expects.

## Installation

Add the crate and `h3` to your `Cargo.toml`:

```toml
[dependencies]
msquic-h3 = "0.0.7"
h3 = "0.0.8"
http = "1"
```

The msquic **native library is loaded dynamically at run time** — it is not
needed to *compile* the Rust code. Choose how the library is provided via a
mutually-exclusive Cargo feature (see [Native library](#native-library) below).

## Quick start

### Client

```rust,ignore
use bytes::Buf;
use msquic::{BufferRef, CredentialConfig, RegistrationConfig, Settings};
use msquic_h3::{Connection, Registration};

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // One registration per process/app; it owns msquic's worker threads.
    let reg = Registration::new(&RegistrationConfig::new().set_app_name("myapp"))?;

    // A configuration describes ALPN + TLS credentials for the connection.
    let alpn = BufferRef::from("h3");
    let config = reg.open_configuration(&[alpn], Some(&Settings::new().set_IdleTimeoutMs(5000)))?;
    config.load_credential(&CredentialConfig::new_client())?;

    // Connect and hand the connection to h3.
    let conn = Connection::connect(&reg, &config, "example.com", 443).await?;
    let (mut driver, mut send_request) = h3::client::new(conn).await?;

    // Drive the connection in the background.
    let drive = tokio::spawn(async move {
        futures::future::poll_fn(|cx| driver.poll_close(cx)).await
    });

    // Send a request and read the response.
    let req = http::Request::builder().uri("https://example.com/").body(())?;
    let mut stream = send_request.send_request(req).await?;
    stream.finish().await?;

    let resp = stream.recv_response().await?;
    println!("status: {}", resp.status());
    while let Some(mut chunk) = stream.recv_data().await? {
        let mut buf = vec![0u8; chunk.remaining()];
        chunk.copy_to_slice(&mut buf);
        // ... use buf ...
    }
    drop(send_request);
    let _ = drive.await;
    Ok(())
}
```

### Server

```rust,ignore
use msquic::{BufferRef, RegistrationConfig};
use msquic_h3::{Listener, Registration};
use std::sync::Arc;

// `config` carries the server ALPN + certificate.
async fn run(config: Arc<msquic::Configuration>) -> Result<(), Box<dyn std::error::Error>> {
    let reg = Registration::new(&RegistrationConfig::default())?;
    let alpn = BufferRef::from("h3");

    // Bind an address (or pass None).
    let mut listener = Listener::new(&reg, config, &[alpn], Some("127.0.0.1:443".parse()?))?;

    while let Some(conn) = listener.accept().await? {
        tokio::spawn(async move {
            let mut h3_conn =
                h3::server::Connection::<_, bytes::Bytes>::new(conn).await.unwrap();
            while let Ok(Some(resolver)) = h3_conn.accept().await {
                // handle each request stream ...
                let _ = resolver;
            }
        });
    }
    Ok(())
}
```

See [`docs/Development.md`](./docs/Development.md) for a runnable end-to-end
walkthrough, and the crate's tests for complete client/server flows.

## Native library

The Rust code compiles without the native library, but linking/running resolves
msquic symbols. Pick **exactly one** provenance feature — they are mutually
exclusive:

| Feature | What it does |
| --- | --- |
| `native-find` | Link/load a system-installed `libmsquic` (the canonical local option). |
| `native-src` | Build msquic from vendored source via cmake (self-contained; used for docs.rs). |

Selecting **neither** is a supported type-check-only configuration (the crate
compiles without linking a native library); selecting **both** is rejected by
msquic's build script. `--all-features` is therefore not a valid build command —
choose one provenance, e.g. `--no-default-features --features native-find`.

### Getting the msquic library at run time

- **Linux:** `sudo apt-get install libmsquic` (loaded dynamically), or download a
  release from [msquic releases](https://github.com/microsoft/msquic/releases).
- **Windows:** PowerShell 7 ships `msquic.dll`; installing pwsh 7 lets the app
  load it. You can also place the DLL anywhere on the library search path.
- **Other:** download a release build and place the library where your app can
  load it.

## Configuring memory budgets

The adapter bounds per-stream memory with finite defaults: **16 MiB per send**,
and **1 MiB + 16384 buffered units per receive stream**. Existing
`Connection::connect` / `Listener::new` callers get these automatically. To tune
them, build an [`H3Config`](https://docs.rs/msquic-h3/latest/msquic-h3/struct.H3Config.html)
and use the additive `*_with_config` constructors:

```rust,ignore
use msquic_h3::{Connection, H3Config};

let cfg = H3Config::builder()
    .with_max_send_bytes(4 * 1024 * 1024) // 4 MiB per send (default 16 MiB)
    .with_max_recv_bytes(512 * 1024)      // 512 KiB per stream (default 1 MiB)
    .with_max_recv_units(4096)            // buffered units per stream (default 16384)
    .build()?;

let conn = Connection::connect_with_config(&reg, &config, "example.com", 443, cfg).await?;
// server: Listener::with_config(&reg, config, &alpn, Some(addr), cfg)?;
```

The receive caps are enforced as backpressure. The send side has no aggregate
cap: per-connection send memory scales as `max_send_bytes × concurrent streams`,
so bound it by choosing `max_send_bytes` and limiting concurrent streams at the
application level. Details in
[`docs/receive-and-send.md`](./docs/receive-and-send.md).

## Documentation

The [`docs/`](./docs/README.md) folder describes the library as built today:

| Document | Covers |
| --- | --- |
| [Architecture](./docs/architecture.md) | Module map, h3 trait boundary, FFI callback model, public API. |
| [Error model](./docs/error-model.md) | How QUIC terminal conditions become `h3` error values. |
| [Receive and send](./docs/receive-and-send.md) | The two data paths and their memory budgets. |
| [Callback safety](./docs/callback-safety.md) | Panic containment and soundness at the FFI boundary. |
| [Registration lifecycle](./docs/registration-lifecycle.md) | Deterministic teardown and drop order. |
| [Testing](./docs/testing.md) | Test layers, seams, and the CI provenance matrix. |
| [Development](./docs/Development.md) | Build, test, and run locally. |
| [Advanced evaluations](./docs/advanced/) | Analyses of msquic preview features (app-owned receive buffers, custom execution). |

API reference: [docs.rs/msquic-h3](https://docs.rs/msquic-h3).

## Building and testing

Verify with a single provenance (see [Native library](#native-library)):

```sh
cargo test  --no-default-features --features native-find
cargo clippy --all-targets --no-default-features --features native-find -- -D warnings
cargo fmt --all -- --check
```

## License

Licensed under the [MIT license](./LICENSE).
