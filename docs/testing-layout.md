# Test source layout

The user's latest instruction on 2026-10-28 moves **all** test implementation out
of production `src/`. This supersedes the earlier colocated-tests/no-`#[path]`
rule. `src/lib.rs` remains the standard library root.

- `tests/unit/` contains private module tests, preserving their original Rust
  module names and `super` scopes through `#[cfg(test)] #[path = "..."]` mounts.
- `tests/common/` contains shared identity/network fixtures and pure test-only
  package installation, format, inspection and trust-anchor helpers. Test-only
  reexports preserve existing fixture call sites without exposing production APIs.
- Integration tests remain at `tests/*.rs`; fixture files and diagnostic programs
  remain under `tests/fixtures`, `tests/tools` and `tests/scripts`.
- `xtask` integration tests already live under `xtask/tests`. Its `src/main.rs`
  and the diagnostic crate's `src/` contain executable code, with no test cases
  or test-only helper implementations left to relocate.

Literal `include_bytes!`/`include_str!` paths are rebased to the same fixture and
SQL files. Production SQL, WAZ bytes, certificate policy, runtime state and host
interfaces are unchanged by the relocation. Source hooks are limited to test
module declarations, reexports, and the code-signing test trust injection call;
the full injected implementation lives under `tests/common`.

Validation uses the same library tests and ignored acceptance suites as before:

```sh
cargo fmt -- --check
cargo test --offline --lib -- --test-threads=4
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline --manifest-path xtask/Cargo.toml
```

The full library suite requires local TCP/UDP socket permission for its existing
loopback tests. Ignored real network/official-artifact suites are still explicit
manual acceptance inputs; relocation does not turn them into passing checks.

Relocation inventory: 48 original test source files moved (45 unit-test files
including the CLI module and 3 shared fixture files). Four additional common
helper modules now hold the 9 former cfg-only functions plus test trust selection,
for 7 files total under `tests/common/`. The original contact, DNS, package,
routing, sandbox, server, setup and CLI Rust module scopes remain in place.

The production source audit finds zero test cases and zero test implementation
files. Its remaining hooks are 19 cfg-gated path module mounts, 5 test-only
reexport groups, and one conditional call into the external test trust helper.
`src/lib.rs` remains unchanged as the library entry. Version metadata and minimum
host hints now identify the user-requested formal `0.9.0` release; historical
Changelog preview entries remain historical records.

Logs for this relocation are under `/private/tmp/`:
`pishoo-tests-relocation-lib.log`, `pishoo-tests-relocation-clippy.log`,
`pishoo-tests-relocation-fmt.log`, `pishoo-tests-relocation-xtask.log`, and
`pishoo-tests-relocation-tools-check.log`. The original-to-new source inventory is
`/private/tmp/pishoo-test-move-inventory.json`.

Final validation after the formal 0.9.0 version update: library 243 tests
(206 passed, 37 unchanged ignored), CLI 3 passed, xtask 14 passed, strict
all-target Clippy and fmt passed, and the diagnostic crate passed an offline
locked check using its synchronized local-host lock entry. No commit, publication
or deployment was performed.
