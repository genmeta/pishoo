# Release preparation for v0.9.0-preview.3

| Package | Published version | Prepared version |
| --- | --- | --- |
| pishoo | 0.9.0-preview.2 | 0.9.0-preview.3 |
| pishoo-common | 0.9.0-preview.2 | 0.9.0-preview.3 |
| pishoo-sdk | 0.1.0-preview.2 | unchanged |

## Scope and compatibility

Require explicit identity configuration, move management/build commands to gmutils,
and remove stopped identities from all local mDNS responders. The host retains its
HTTP APIs and reload command. Public address records stop renewing and expire
naturally; no withdrawal request is sent.

Use gmutils v0.9.0-preview.3 for the migrated commands. Its identity activation
creates config.db, while browser-only application leaves it absent. Host package
installation does not invoke or depend on gmutils. Build/publication validation
uses the publisher identity's running Pishoo API. Business WEBC packages remain
independent and are never bundled or installed automatically.

## Validation

Run nightly formatting, commitlint, locked workspace tests, documentation, MSRV,
and DEB/RPM/Homebrew dry runs before tagging. Keep the registry Cargo.lock and
existing published dhttp 0.7.1, dhttp-home 0.6.1 and daccess 0.1.1 dependencies.
Run Cargo with --manifest-path from outside ancestor local dependency overrides;
all sources remain in the current checkout. Do not commit local path overrides.

The identity lifecycle and multicast regressions include a valid browser identity
without config.db, disabled records on multiple mDNS instances, and blocked guest
cleanup. Use a dedicated test identity for AnySee acceptance; distinguish remote
same-name services and cached answers from new local publication.

## Publication

After successful CI and rebase merge, create annotated tag v0.9.0-preview.3 at the
checked commit. The tag triggers Linux DEB/RPM and macOS Homebrew preview artifacts.
The native package and pishoo-common versions must match. No pishoo-pack binary is
shipped. The SDK workflow skips its already-published unchanged version.

Existing package signing, public APT verification and SDK authentication policies
remain unchanged. Windows publication remains deferred. This preparation does not
create a tag, publish artifacts or deploy/restart production services.

## Preparation results (2026-10-15)

- Locked workspace tests: 137 passed, 30 explicit/manual tests ignored.
- Release-contract and service-installation tests: 14 passed.
- Nightly formatting and Conventional Commits checks passed locally.
- Platform builds, documentation/MSRV and artifact dry runs remain CI gates.
