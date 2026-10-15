# Release preparation for v0.9.0-preview.1

| Package | Previous published version | Prepared version |
| --- | --- | --- |
| `pishoo` | 0.8.2 | 0.9.0-preview.1 |
| `pishoo-common` | 0.8.2 | 0.9.0-preview.1 |
| `pishoo-sdk` | first release | 0.1.0-preview.1 |

## Dependency order

dquic → h3x → dhttp → ddns → daccess

Manifest versions use Cargo's compatible-version requirements, without `=`.
They allow patch updates within the selected 0.x series. External Genmeta
dependencies come from crates.io; no sibling checkout or Git patch is required.
The dquic TLS backend is the published qrustls 0.23.45 package, used as `rustls`
where the application exchanges TLS types with qtls.

## Validation and lockfiles

This PR is release preparation. Unpublished upstream versions can make CI fail;
such failures do not confirm compilation, tests or package verification.
For tracked lockfiles, run `cargo generate-lockfile` after upstream publication,
review and commit the resulting registry lockfile before tagging.
Run Cargo from a checkout outside directories containing ancestor path patches.
Never commit a lockfile generated with local transport path overrides as the
registry release lockfile. Re-run the normal CI checks and publish dry run after
upstream publication and lockfile refresh; merge and tag only that checked commit.

## Publication

Planned tag: `v0.9.0-preview.1`. Release through the repository's GitHub Actions workflow.
Use an annotated tag with the changelog entry as its release notes. Actual
publication and production deployment remain separate from this preparation PR.

The legacy gateway stub is not published. pishoo-common tracks 0.9.0-preview.1
(0.9.0~preview.1-1 for DEB/RPM). publish-sdk.yml publishes pishoo-sdk 0.1.0-preview.1 from wit/
on the same release tag; configure CARGO_REGISTRY_TOKEN for its first upload,
then configure Trusted Publishing for genmeta/pishoo and publish-sdk.yml.

## Tag-triggered artifacts

Pushing `v0.9.0-preview.1` to this repository triggers release.yml for DEB, RPM and Homebrew.
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
