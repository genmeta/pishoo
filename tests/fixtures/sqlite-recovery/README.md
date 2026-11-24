# SQLite recovery probe

The fixture uses SQLite's WASI unix-dotfile VFS. Its HTTP routes either read
the database, exhaust fuel after spilling dirty pages, or return headers and
wait while a write transaction is open. Its CLI command traps after a spill.

With the WASI SDK and the Rust `wasm32-wasip2` target installed, build from
this directory:

```sh
export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/wasm32-wasip2-clang"
export AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar"
export CFLAGS_wasm32_wasip2="-USQLITE_THREADSAFE -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_WAL -DSQLITE_TEMP_STORE=3"
cargo build --locked --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/sqlite_recovery_probe.wasm probe.wasm
```
