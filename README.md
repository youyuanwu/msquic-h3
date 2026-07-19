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

# License
This project is licensed under the MIT license.