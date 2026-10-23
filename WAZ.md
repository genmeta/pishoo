# WAZ v1 package format

A `.waz` release is a standard ZIP archive with MIME `application/zip`. It can be
listed and extracted with ordinary ZIP tools. Publication and installation accept
this format only; previous WEBC release files must be rebuilt from source.

## Files

```text
manifest.json       Required package metadata
signature.json      Required publisher signature and certificate chain
backend.wasm        Optional WASI HTTP Component, with embedded OpenAPI
public/index.html   Required when a frontend is present
public/...          Frontend files
README.md           Optional documentation
CHANGELOG.md        Optional documentation
```

At least a backend or frontend must be present. Jade automatically includes the
project root's `README.md` and `CHANGELOG.md` when they are regular files. Missing
documents are skipped, and `jade.toml` needs no document declarations. Documents
are part of the signed release, but are not frontend resources or an OpenAPI
source. The existing `release-notes` setting remains a short release summary.

## Manifest

`manifest.json` is UTF-8 JSON. It is the release metadata authority:

```json
{
  "format": "waz/1",
  "package": {
    "author": "alice.smith.dhttp.net",
    "name": "hello",
    "version": "0.1.0",
    "title": "Hello",
    "description": "Hello application",
    "license": "Apache-2.0",
    "release-notes": "Initial release"
  },
  "dependencies": [],
  "backend": "backend.wasm",
  "frontend": "public"
}
```

All shown package fields are required; `repository` is an optional HTTPS URL.
Author is a canonical DHTTP identity, name is a package short name, and version is
`MAJOR.MINOR.PATCH`. Dependencies use exact `{ "author", "name", "version" }`
objects, with at most 100 entries. The optional `backend` and `frontend` fields
must match the actual runtime files and use the fixed paths shown above. Unknown
manifest and package fields are rejected.

## ZIP profile and limits

Jade writes Deflate compression at level 6, sorted file names, a fixed ZIP
timestamp and regular-file permissions. Readers accept Stored or Deflate entries.
Only regular file entries are used; directory structure is inferred from paths.
Encryption, symlinks, device files, duplicate paths, traversal segments, absolute
paths, ZIP64, multi-disk archives, data descriptors, self-extracting prefixes and
archive comments/trailing data are rejected. Local headers must agree with the
central directory, and all archive bytes must belong to indexed ZIP structures.

The compressed archive and total decompressed file bytes are each limited to
64 MiB. The manifest is limited to 1 MiB and signature envelope to 64 KiB. A
frontend or document file is limited to 16 MiB. Runtime content and documents share
the existing 4096-entry and 32-directory-depth budgets; paths are at most 1024
UTF-8 bytes. Limits are checked before and during decompression.

## Signature and digests

`package_sha256` is SHA-256 of the complete ZIP bytes. `payload_sha256` identifies
the uncompressed signed files, independent of ZIP compression and timestamps.

To compute the payload digest, initialize SHA-256 with the ASCII bytes
`pishoo-waz-payload-v1` followed by NUL. Visit every regular file except
`signature.json`, sorted by its UTF-8 path bytes, and append:

1. Path byte length as an unsigned 32-bit little-endian integer.
2. The path bytes, without a leading slash.
3. Uncompressed content length as an unsigned 64-bit little-endian integer.
4. The 32 raw SHA-256 bytes of the file content.

The raw contents of `manifest.json`, including its JSON formatting, participate
like every other file. There are no excluded runtime or document files.

The publisher signs `pishoo-waz-signature-v1` followed by NUL and the 32 raw payload
digest bytes. The signature envelope is JSON with exactly `version: 1`,
`certificates` (a nonempty array of base64 DER certificates), and `signature`
(base64 signature bytes). Identity binding and supported signature algorithms
retain the existing DHTTP certificate verification rules. ZIP CRC checks do not
replace signature verification.

The host may project verified ZIP contents into its existing internal resource
container for installation and execution. That internal representation is not a
release file and is never accepted as a substitute for a ZIP at publication or
installation boundaries. Store and host readers must be updated before deploying
new ZIP releases; no automatic conversion or re-signing of old releases occurs.
