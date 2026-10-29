# Offline developer signature policy (2026-10-29)

There is one developer signature verification rule set, without a policy version
or a dedicated capability header. The related signing schemas have not shipped;
revise their original definitions directly without draft-to-draft migrations.

Minimum host: Pishoo 0.9.0, reported through `Pishoo-Version`.
CertServer participates in purchase, certificate issuance and renewal only.
Jade, Registry and Pishoo do not query `/v2/cert` during package publication or
installation. Entitlement is represented by the signed codeSigning certificate's
validity period. Publication still verifies the pinned DHTTP Root, strict
P-384/codeSigning leaf and issuer profiles, package signature, canonical author,
TLS uploader ownership and immutable complete-WAZ digest. It checks certificate
validity again immediately before commit. Cancellation or payment changes do not
retroactively change already issued credentials.

## Portable publication receipts

The still-unreleased signing schema includes signed receipts directly in schema v7
`versions.publication_receipt`. The receipt contains base64 `payload`, base64 DER
`signature`, and base64 DER `certificates` for the Registry's TLS signing identity.
The signed input is `pishoo-publication-receipt-v1\0` followed by the exact decoded
payload bytes. The payload is trusted only after verification; reserialization is
not used to verify signatures. The payload is exactly:

```json
{"format":"pishoo-publication/1","registry":"store.dhttp.net","registry_owner_hash":"<pinned owner>","author":"<canonical author>","name":"<package>","version":"<version>","package_sha256":"<complete WAZ SHA256>","size":123,"author_owner_hash":"<owner>","leaf_fingerprint_sha256":"<developer leaf SHA256>","issuer_id":"<developer issuer DER SHA256>","verified_at":1234567890}
```

The Registry signs with its existing local authority; its private key never
enters a package, database or export. No new Registry resource fields are added.
The installer authenticates the receipt certificate path to the compiled Root at
`verified_at`, requires serverAuth, the pinned Registry name and owner, and checks
the signature and all payload bindings before using the historical time. It then
verifies the package and developer certificate chain at that trusted time.
Developer-supplied timestamps and unsigned JSON times never establish history.
Receipts can travel with metadata through mirrors or files without CertServer.
The existing install API still downloads through the pinned Registry; this change
does not add a CLI for installing an arbitrary local archive or mirror.

Normal developer-certificate expiry does not invalidate an accepted historical
release. Same-version publication remains exact-byte idempotent; a new signature
or ZIP with different bytes is a conflict. Unsigned legacy records remain auditable but cannot be newly installed without developer publication evidence. Migration preserves
all old rows and leaves receipts NULL; it does not upgrade trust or sign history.
Use `deploy/aws/audit-code-signing.py DB PACKAGES --migrate-v6 --service-stopped
--backup-directory NEW_DIR` or the backed-up deployment's UPGRADE_REGISTRY_V7=1.
No production migration, deployment or re-signing is part of this implementation.

## Independent revocation artifacts

CertServer exposes `GET /v2/cert/revocations/{issuer_id}` as application/pkix-crl.
These are issuer-signed complete DER CRLs, with AKI, monotonic PostgreSQL-backed
CRLNumber (sequence included in the unreleased V15 definition), thisUpdate and nextUpdate (24 hours). Exports are serialized per issuer
across replicas. They contain every revoked serial for that issuer, including
expired certificates because historical packages remain verifiable. The issuer
must permit cRLSign; there is no fallback to another key or TLS issuer.

CRL distribution is independent of signing/publication/installation. Operators
can fetch and mirror these public artifacts separately or import them as files:

- Jade: `<author profile>/trust/code-signing/<issuer DER SHA256>.crl`.
- Registry: `<registry profile>/store/packages/<issuer DER SHA256>.crl`.
- Pishoo: `<identity profile>/trust/code-signing/<issuer DER SHA256>.crl`.

Registry attaches an imported list as base64 `revocation_list` metadata; its
receipt remains immutable. Pishoo verifies list signatures using the package's
already trusted issuer and caches authenticated newer lists under the existing
identity installation lock. A lower CRLNumber cannot overwrite a newer cache.
A received valid revocation is retained even when installation is refused.
Known revoked leaves remain denied even if their list subsequently expires.
Stale lists deny new installation/publication with signing_revocation_list_stale;
malformed, foreign-issuer or unsupported critical/delta/indirect lists are rejected
as signing_revocation_list_invalid. Missing lists permit offline verification and
mean current revocation is unknown. No claim of immediate global revocation is
made without distribution of new evidence. There is no automatic CertServer
refresh task, online fallback, or automatic runtime stop of installed apps.

## Installation guarantees

Every exact dependency, including a reused installed version, must pass receipt,
package, author continuity and available-CRL verification. The whole closure is
verified before staging; the same snapshot is compiled and staged, with final
local CRL checks before the existing atomic commit. No issuer network operation
is performed while holding installation/SQLite locks. Business data and ACL rules
retain their existing semantics. Ownership cannot be seeded by a new release
when the installed owner is unknown; verified evidence for the old version is
still required. TLS identity OCSP/bootstrap/DNS behavior is a separate transport
concern and is unchanged by this code-signing policy.

## Production build boundary

The Registry name and owner overrides (`PISHOO_REGISTRY_NAME`,
`PISHOO_REGISTRY_OWNER_HASH`) are compile-time debug-build inputs. A build with
debug assertions disabled rejects either variable when present, including an
empty value. Ordinary release builds retain the official pinned Registry. These
variables are never read at runtime.

`DHTTP_HOME` remains an explicit runtime choice for non-root user processes; when
unset, identity discovery and logging use the ordinary user home. Linux root
services use the existing global-home selection. The official release workflow
does not supply sandbox Registry overrides. Test-root builds remain separate
from the normal release target directory.
