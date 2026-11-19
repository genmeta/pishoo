# WAZ package format

## WAZ v2

New Jade builds use `format = "waz/2"` in a UTF-8 `manifest.toml` inside a standard
ZIP archive. The MIME type is `application/zip`.

```text
manifest.toml
wasm/<full-package-name>.wasm
web/index.html
web/...
docs/manual.md
docs/...
```

A package contains a backend, a frontend, or both. A backend named `std/comm/message`
uses `wasm/std/comm/message.wasm`. Frontend resources use `web/`. Markdown documents
use `docs/` and are collected recursively from the source project's `docs/`
directory, respecting project ignore rules. The main manual entry is `docs/manual.md`.

## Release manifest

```toml
format = "waz/2"
wasm = "wasm/std/comm/example.wasm"
web = "web"
dependencies = []

[package]
author = "dev.genmeta.tech"
name = "std/comm/example"
version = "0.1.0"
title = "Example"
description = "Example package"
license = "Apache-2.0"
release-notes = "Initial release"
repository = "https://example.com/source"

[docs]
manual = "docs/manual.md"
readme = "https://example.com/readme"
changelog = "https://example.com/changelog"

[signature]
version = 1
certificates = ["<Base64 DER leaf>", "<Base64 DER issuer>"]
value = "<Base64 signature>"
```

`format` identifies the release layout and signing rules. `package.version`
identifies the software release. The package author remains the authenticated
publisher identity. New builds use the short author name in the manifest. Readers
expand it with `.dhttp.net` for identity checks and accept existing full names. Package names support slash-separated segments and retain
the existing 63-byte total limit and segment validation.

Package fields shown above are required except `repository`. `wasm` and
`web` are optional individually and must exactly match the runtime files.
Readers also accept earlier v2 manifests using `backend` and `frontend`.
The two naming schemes cannot be mixed in a manifest.
Unknown manifest, package, and document fields are rejected.

`dependencies` is an explicit array of exact name/version objects, with at most
100 entries. A nonempty TOML dependency list may use tables.

```toml
[[dependencies]]
name = "std/profile"
version = "0.3.1"
```

Local dependency `path` values belong to `jade.toml` and are omitted from releases.
Historical author-qualified dependencies retain their exact identity checks.
Package-name ownership and repeated-version publication follow the existing
store contract.

Standard Libs and combined frontend/backend packages use complete names beginning
with `std/`. Pure frontend Apps such as `workspace` and `introduction` retain
their App names. New `std/` names require a standard publisher. The host maps the
complete namespace once in runtime and data paths. Historical standard package
names continue using their original implicit namespace. Business API paths are
declared independently in the embedded OpenAPI document.

`docs` is optional. `manual`, when declared, is exactly `docs/manual.md` and must
exist in the archive. `readme` and `changelog` are optional HTTPS URLs with hosts
and without credentials. Source `jade.toml` accepts the same `[docs]` declaration.
Jade also declares `docs.manual` automatically when `docs/manual.md` exists.
Root README and CHANGELOG files remain project documentation. Their release
entry points are the URLs in `[docs]`.

## Manuals

Manuals use English UTF-8 CommonMark Markdown and conventional man-style headings.
Use NAME, SYNOPSIS, DESCRIPTION, API or OPERATIONS, PERMISSIONS, EXAMPLES, ERRORS,
FILES, COMPATIBILITY, and SEE ALSO as applicable. Describe actual request shapes,
identity checks, failure behavior, lifecycle, and persistent files. Additional
Markdown topics may be linked relative to the main manual.

The backend embeds its OpenAPI document. The manual explains operation semantics
and workflows and accompanies the corresponding package version.

## Signature boundary

A signed v2 release ends its manifest with a literal, unindented `[signature]`
table header on its own line. The table has exactly `version`, `certificates`,
and `value`. Signature envelope version 1 retains the existing developer
certificate and cryptographic verification policy. This envelope version and
the WAZ format version describe separate objects.

The entire `signature` table is excluded from the payload digest. For the
manifest entry, the signed content is the original UTF-8 bytes from byte zero
to the first byte of the final `[signature]` header. This includes all preceding
comments, whitespace, and newlines. A change to those bytes changes the payload.

Readers locate the actual signature table using the TOML parser's source span.
They parse the complete document and independently parse its prefix and suffix. The prefix must equal the complete document with `signature` removed.
The suffix must contain only that same signature table. This verifies the byte
boundary and the last-field rule, including quoted strings and multiline values.
Duplicate TOML keys, ambiguous header positions, and content outside the signature
table in the suffix are rejected.

An unsigned build omits the signature table and ends its manifest with a newline.
`jade publish` signs that exact existing snapshot and appends the signature table.
An already signed snapshot is verified and uploaded with its existing bytes.

## Digests

`payload_sha256` hashes the uncompressed release entries. Initialize SHA-256 with
the ASCII bytes `pishoo-waz-payload-v2` followed by NUL. Visit all regular files in
UTF-8 path-byte order. For each entry append the following bytes.

1. Path length as an unsigned 32-bit little-endian integer.
2. The UTF-8 path without a leading slash.
3. Content length as an unsigned 64-bit little-endian integer.
4. The 32 raw SHA-256 bytes of the content.

For `manifest.toml`, content and content length use the prefix defined above.
Every WASM, frontend resource, and manual participates with its original bytes.

The publisher signs the ASCII bytes `pishoo-waz-signature-v2` followed by NUL and
the 32 raw payload-digest bytes. `signature.value` is the Base64 signature.
Certificate chain and identity validation use the existing developer trust rules.

`package_sha256` hashes the complete final ZIP bytes, including the full manifest
and its signature. Store receipts and download checks bind that exact archive.

## ZIP profile and limits

Jade uses Deflate level 6, sorted entry names, a fixed ZIP timestamp, and regular
file permissions. Readers accept Stored and Deflate compression. Directories
are inferred from file paths.

The compressed archive and total decompressed contents each have a 64 MiB limit.
The complete manifest has a 1 MiB limit and its signature suffix has a 64 KiB
limit. A backend has the package-wide limit. Each frontend resource or manual has
a 16 MiB limit. The archive has at most 4096 file entries and 32 directory levels.
Paths have at most 1024 UTF-8 bytes.

Readers reject encrypted entries, symlinks, special files, duplicate paths,
file/directory conflicts, traversal and absolute paths, ZIP64, multi-disk archives,
data descriptors, self-extracting prefixes, archive comments, and trailing bytes.
Local headers agree with the central directory. All archive bytes belong to
indexed structures. CRC checks accompany the publisher signature checks.

## Historical WAZ v1

Readers continue accepting `format: "waz/1"` in `manifest.json`, a separate
`signature.json`, `backend.wasm`, `public/` resources, and optional root `README.md`
and `CHANGELOG.md`. V1 files retain the original payload and signature domain
strings ending in `v1`, the JSON signature envelope's `signature` value key, and
their original signed bytes.

Each archive has exactly one release manifest. V1 and v2 runtime/document layouts
are validated against the selected format. Old published archives and their
receipts continue identifying their original content.

Jade and the store may project validated releases into their existing internal
resource representation. The host installs the runtime files from the verified
release. Internal resource projections retain their existing API responsibilities.

## Installed manual access

For v2 installations, Pishoo retains the verified original archive in its local
`.volume/sys/pkg/releases/<package_sha256>.waz` cache. Archive publication is
atomic and occurs before the installation transaction commits. Package detail
reads check the retained bytes against the installed digest before returning
`docs` locations and the English manual text through the existing package detail
response. Historical installations without a retained archive remain readable.
The cache may retain earlier downloaded releases.

The store returns `docs` locations and the manual text on exact release details.
Workspace exposes the README and CHANGELOG links in its existing package views.
The bundled manual is available through package contents and backend detail reads.
