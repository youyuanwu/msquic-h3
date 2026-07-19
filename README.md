# msquic-h3
Use [msquic](https://github.com/microsoft/msquic) in rust with [h3](https://github.com/hyperium/h3).

Experimental.

Currently can run h3 client and server.

# Design docs
See [`docs/`](./docs/README.md) for the design documentation describing the
library's architecture, error model, receive/send paths, callback safety,
registration lifecycle, and testing.

# Build
msquic lib is dynamically loaded and it is not required for building the rust code.

# Test and run
## Posix
Install msquic pkg:
```sh
sudo apt-get install libmsquic
```
Rust code will load the lib dynamically.
## Windows
pwsh 7 packages the msquic.dll. If you install pwsh 7, rust will load it from there.

## Other
You can download msquic from github release [here](https://github.com/microsoft/msquic/releases) and put the lib to locations where app can load it.

# Configuring memory budgets
The adapter bounds per-stream memory with fixed finite defaults (16 MiB per send,
1 MiB and 16384 buffered units per receive stream). Existing `Connection::connect`
/ `Listener::new` callers get these defaults automatically. To tune them, build an
`H3Config` and use the additive `*_with_config` constructors:

```rust,ignore
use msquic_h3::{Connection, H3Config};

let cfg = H3Config::builder()
    .with_max_send_bytes(4 * 1024 * 1024) // 4 MiB per send (default 16 MiB)
    .with_max_recv_bytes(512 * 1024)      // 512 KiB per stream (default 1 MiB)
    .with_max_recv_units(4096)            // buffered units per stream (default 16384)
    .build()?;

let conn = Connection::connect_with_config(&reg, &config, "example.com", 443, cfg).await?;
// server side: Listener::with_config(&reg, config, &alpn, Some(addr), cfg)?;
```

The receive caps are enforced as backpressure. The send side has no aggregate
cap: per-connection send memory scales as `max_send_bytes × concurrent streams`,
so bound it by choosing `max_send_bytes` and limiting concurrent streams at the
application level. See [`docs/receive-and-send.md`](./docs/receive-and-send.md).

# License
This project is licensed under the MIT license.