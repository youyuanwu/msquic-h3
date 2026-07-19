# msquic-h3 design docs

`msquic-h3` is a Rust adapter that lets an application drive
[Microsoft's msquic](https://github.com/microsoft/msquic) QUIC stack through
[hyperium's `h3`](https://github.com/hyperium/h3) HTTP/3 transport traits. These
documents describe the library as it is built today.

Start with the architecture overview for the map of the whole crate, then dive
into the subsystem that interests you.

| Document | What it covers |
| --- | --- |
| [Architecture](./architecture.md) | The map of the library: purpose and h3 trait boundary, module layout, public API surface, the FFI callback model, and the h3 trait mapping. |
| [Error model](./error-model.md) | How QUIC terminal conditions become `h3` error values: the h3 error contract, the adapter-owned terminal enums, terminal-cause slots with commit-on-delivery and refinement, and the pure send reducer. |
| [Receive and send](./receive-and-send.md) | The two data paths: per-stream receive backpressure and the owned-buffer send path with its exactly-once ownership transfer. |
| [Callback safety](./callback-safety.md) | Keeping the FFI boundary sound: panic containment, poison/recover, take-ownership-only-on-success, and native stream teardown on drop. |
| [Registration lifecycle](./registration-lifecycle.md) | Deterministic teardown: registration rundown tracking, the `wait_idle` future, and the required drop order. |
| [Testing](./testing.md) | The test layers, injectable seams, native-library attestation gate, canonical commands, and the CI provenance matrix. |
| [Development](./Development.md) | How to build, test, and run locally. |
