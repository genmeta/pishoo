# Release preparation for v0.9.0-preview.2

| Package | Previous published version | Prepared version |
| --- | --- | --- |
| `pishoo` | 0.9.0-preview.1 | 0.9.0-preview.2 |
| `pishoo-common` | 0.9.0-preview.1 | 0.9.0-preview.2 |
| `pishoo-sdk` | 0.1.0-preview.1 | 0.1.0-preview.2 |

## Dependency order

Release prerequisites are [dhttp PR #34](https://github.com/genmeta/dhttp/pull/34)
(dhttp-home 0.6.1, then dhttp 0.7.1) and
[daccess PR #3](https://github.com/genmeta/daccess/pull/3) (daccess 0.1.1).
Both releases must be available on crates.io before final registry checks.
The manifests require these versions with Cargo's compatible-version requirements.
Existing dyns 0.8.0, h3x 0.7.0 and QUIC versions remain in their current series;
the TLS backend remains qrustls 0.23.45. No sibling path or Git patches are shipped.

## Validation and lockfiles

The required dhttp 0.7.1, dhttp-home 0.6.1 and daccess 0.1.1 releases are
available on crates.io. The committed Cargo.lock now resolves those registry
versions with checksums and no local overrides. Registry checks run from a
checkout outside ancestor Cargo path patches.

Run locked Rust CI, documentation, MSRV and crate/DEB/RPM/Homebrew dry runs
before tagging. Merge using rebase and tag only the checked commit.
Local integration lockfiles must never replace the registry release lockfile.

## Publication

Planned tag: `v0.9.0-preview.2`. Release through the repository's GitHub Actions workflow.
Use an annotated tag with the changelog entry as its release notes. Actual
publication and production deployment remain separate from this preparation PR.

The legacy gateway stub is not published. pishoo-common tracks 0.9.0-preview.2
(0.9.0~preview.2-1 for DEB/RPM). publish-sdk.yml publishes pishoo-sdk 0.1.0-preview.2 from wit/
on the same release tag. SDK publication uses the repository token when configured, otherwise Trusted
Publishing for genmeta/pishoo and publish-sdk.yml; first publication requires a token.

## Tag-triggered artifacts

Pushing `v0.9.0-preview.2` to this repository triggers release.yml for DEB, RPM and Homebrew.
The tag must match the main package version. Packaging creates a draft GitHub
Release, uploads artifacts, and publishes that Release after all package jobs
succeed. Release builds keep `--locked`; refresh the registry lockfile after
upstream publication before tagging. The existing S3, signing and tap/bucket
credentials must be configured in repository Actions settings.

Windows publication is deferred for this release. Tag-triggered binary
artifacts cover Linux DEB/RPM and macOS Homebrew.

## Business App/Lib publication

Host DEB, RPM and Homebrew packages deliver the native binaries and service files.
They do not require or bundle Workspace, Profile or Message WEBC files, invoke
gmutils, or install business packages at first startup. `pishoo-common` carries
the Linux service and shared infrastructure files; its version remains paired
with the host package.

Publish the signed business Libs and App independently to the store, with the
exact dependencies declared by the App available before users install it.
After installing Pishoo and a gmutils version with `genmeta package install`, configuring
an identity and starting its Pishoo service, that identity's account runs
`genmeta package install std.dhttp.net/workspace --id NAME`. Installation calls the running
Pishoo API and recursively installs exact Lib dependencies. Host package install
order does not matter, and Pishoo has no gmutils package dependency. Store
publication and this user-triggered install remain separate from the host release.

Pishoo, pishoo-common and pishoo-sdk are preview prereleases. DEB/RPM and
Homebrew publication selects the preview channel from each package version.

## History and validation

The 14 development commits after snapshot `66a339a` are rebased onto main
(`7c91d2b`), which already contains the earlier development history as the
preview.1 squash. Documentation cleanup, formatting and release preparation add
three commits, for 17 commits above main. The release commit updates versions and
dependencies; temporary task plans and implementation reports are excluded.
Required registry checks and publication ordering above still apply.

## Preparation checks (2026-10-15)

- Workspace tests: 159 passed, 29 opt-in/recorded-environment tests excluded.
- Release-contract and service-installation tests: 14 passed.
- Workspace production build and 23 frontend tests passed.
- Nightly formatting and diff checks passed.

The preparation checks above used fixed upstream PR sources. Final checks use
published crates through the refreshed registry lockfile. Their CI results must
pass before merging and tagging.

Publication uses the repository CARGO_REGISTRY_TOKEN when configured. Without
that secret, existing crates use Trusted Publishing; first publication requires
an API token.
