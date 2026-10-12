# Release preparation for v0.9.0

| Package | Previous published version | Prepared version |
| --- | --- | --- |
| `pishoo` | 0.8.2 | 0.9.0 |
| `pishoo-common` | 0.8.2 | 0.9.0 |
| `pishoo-sdk` | first release | 0.1.0 |

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

Planned tag: `v0.9.0`. Release through the repository's GitHub Actions workflow.
Use an annotated tag with the changelog entry as its release notes. Actual
publication and production deployment remain separate from this preparation PR.

The legacy gateway stub is not published. pishoo-common tracks 0.9.0
(0.9.0-1 for DEB/RPM). publish-sdk.yml publishes pishoo-sdk 0.1.0 from wit/
on the same release tag; configure CARGO_REGISTRY_TOKEN for its first upload,
then configure Trusted Publishing for genmeta/pishoo and publish-sdk.yml.

## Tag-triggered artifacts

Pushing `v0.9.0` to this repository triggers release.yml for DEB, RPM and Homebrew.
The tag must match the main package version. Packaging creates a draft GitHub
Release, uploads artifacts, and publishes that Release after all package jobs
succeed. Release builds keep `--locked`; refresh the registry lockfile after
upstream publication before tagging. The existing S3, signing and tap/bucket
credentials must be configured in repository Actions settings.

Windows publication is deferred for this release. Tag-triggered binary
artifacts cover Linux DEB/RPM and macOS Homebrew.
