# Local Lib HTTP probe

This standalone WASI component calls `/profile/profile/context` (the ordinary `profile` Lib declares `/profile/context`). Its HTTP response records the original caller and the dependency response; its CLI export persists the same result under its own `/db`. The real host acceptance test checks owner success, visitor denial in the downstream Lib, and `caller=None` in CLI while its newly initiated HTTP request is authenticated by actual QUIC credentials.

Build outside the parent transport override configuration:

```sh
cd /tmp
cargo build --locked --release --target wasm32-wasip2 --manifest-path /path/to/pishoo/pishoo/tests/fixtures/lib-http-probe/Cargo.toml
```

Run `dependent_libs_use_standard_local_http_with_real_caller_identity` alone with `--ignored`, after building `profile-lib` and `message-lib`. `PISHOO_LIB_HTTP_PROBE` may override the component path. The test owns process-wide TLS roots, DNS and home, uses isolated credentials/databases, and never installs anything into the user's home.
