# Pishoo test snapshot — 2026-10-09

Pishoo uses `feat/pishoo-integration`; each dependency has a branch named for its change. Pishoo's manifests and `Cargo.lock` pin the dependency commits below; sibling checkouts and the developer's parent-directory Cargo patches are not required.

| Repository | Branch | Commit | Included changes |
| --- | --- | --- | --- |
| dhttp | `fix/client-ocsp-compatibility` | `0a1464941021dcf3f9c0161d2bc5d42cb47ccbf1` | Optional client OCSP listener integration, network recovery documentation, pinned transport dependencies |
| dquic | `fix/endpoint-kind-matching` | `4acdce041ee3b6d1bb1c0277462e3783028755da` | Select existing Direct/Mediate endpoints of the same kind, preserve local relay mappings, log DNS candidates and rejection reasons |
| ddns | `chore/h3-dns-diagnostics` | `65487e4b5d8bc35029595de382fb835d0b52ebff` | Log H3 lookup attempts, HTTP response status and semantic failures |
| h3x | `fix/qpack-feedback-backpressure` | `cd1ed3aea181d1f05f1627564f88a5e2d33aaee5` | Bounded QPACK feedback, insertion-count coalescing, ACK backpressure, streaming/control progress and regression tests |
| daccess | Existing fixed revision | `cf8f72f4e6bedbd7c98648ffee31053cb509b395` | Existing authorization dependency; unchanged in this snapshot |
| rustls fork | Existing fixed revision | `22dec513c4ebdf89f113f46e100393cf18963aa6` | Existing qtls dependency; unchanged in this snapshot |

Pishoo includes explicit `/file` proxy overrides, HA WebSocket/resource acceptance commands, related tests and documentation. Local HAR captures, identity files, databases, runtime logs and `target/` artifacts are not part of the snapshot.

## Build and run

Use Rust 1.97.1 (the checked-in toolchain) and Bun 1.4.2 on PATH. Git access to the pinned repositories is required on the first build.

```sh
cargo build --locked -p pishoo --bin pishoo
cargo test --locked --workspace
DHTTP_HOME=/path/to/identity-home ./target/debug/pishoo
```

Pishoo loads identities and configuration at startup. Restart after changing identities, configuration or Libs. Configure local HTTP/TCP proxies through the existing management commands/API; contact and access rules remain managed by daccess.

## Verification and useful diagnostics

The endpoint-selection regression checks that different relay agents retain registered local endpoints for every NAT classification and that the returned local endpoint resolves to the original UDP socket. Direct and mediated endpoints no longer cross-match. Address-family, network-scope and mDNS interface filtering remain covered.

The local qprotocol suite passed 95 tests. Against the locked Git dependency graph, the Pishoo workspace passed 131 tests (124 library tests and 7 integration tests), with 10 tests skipped under their existing conditions. The workspace all-targets compilation check and modified Rust source formatting checks also passed.

To inspect live DNS and candidate-path selection:

```sh
RUST_LOG=warn,pishoo=info,dhttp=debug,ddns=debug,qconnection=debug,qtls=debug \
  DHTTP_HOME=/path/to/identity-home ./target/debug/pishoo
```

Runtime logs include DNS sources and endpoints, candidate paths, socket-registration rejection reasons, and H3 lookup response status. They distinguish failure before TLS from an HTTP authorization refusal after a successful connection. A contact application returning 403 requires an appropriate `POST /contact` policy on the receiving identity.
