
```sh
# This is required to run tests
export LD_LIBRARY_PATH="$(pwd)/build"
```

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