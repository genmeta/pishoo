# Static file HTTP behavior

Pishoo serves installed App files at `/app/<name>/…` and native files at
`/std/file/…`. Both support conditional GET/HEAD and single byte ranges. File
selection, directory confinement, App redirects and page deep-link fallback keep
their existing rules. Every network request passes through current authorization
before a file is selected or a conditional response is generated.

## Cache policies

| Response | Cache-Control |
| --- | --- |
| Installed App and native file GET, including 206 and 304 | `private, no-cache` |
| Native file HEAD | `private, no-cache` |
| App HEAD, which exposes the current verified viewer | `no-store` |
| Local development App | `no-store` |
| Static redirects and errors; authorization failures | `no-store` |

`private, no-cache` allows a browser to store bytes, but requires validation before
reuse. It reduces repeated transfers, while retaining a network round trip.
Pishoo adds no file-content cache, background refresh task or cache configuration.
It does not infer long-term caching from asset names or extensions.

These policies apply to host-served static files. WASM responses, including
Profile avatars, retain the cache semantics supplied by their producer; proxies
continue forwarding conditional and cache headers. Avatar caching and immediate
display across Workspace refreshes require separate Profile/Workspace changes.

## Validators and conditions

Responses carry a weak ETag derived from metadata of the opened file: device,
inode, length, mtime and ctime, including their available nanoseconds. This does
not scan file contents or assert a content digest. Last-Modified is supplied when
the modification time can be represented, rounded to seconds and capped at Date.
An atomic replacement changes the tag even when length and mtime are preserved.

Conditions are evaluated after normal routing and authorization checks:

1. If-Match uses strong comparison; otherwise If-Unmodified-Since applies.
2. If-None-Match uses weak comparison; otherwise If-Modified-Since applies.
3. Range is considered only for GET that would otherwise return 200.

If-Match `*` succeeds for an existing file. A specific tag cannot strongly match
the host's weak validator and returns 412. If-None-Match supports lists, repeated
fields, weak or strong forms of the same opaque tag, and `*`. Invalid entity-tag
conditions return 400; invalid or repeated single-value dates are ignored.

A 304 has no body. Its Content-Length describes the corresponding full 200
representation, preventing Axum from automatically adding a misleading zero for
nonempty files. App conditional HEAD still carries the current verified
Pishoo-Viewer-Name and `no-store`.

Page fallback uses the selected `index.html` file's validators and media type.
Missing resources, redirects and unsupported methods retain 404, 307 and 405
respectively, regardless of conditions.

## Byte ranges

For a ten-byte file:

| Request | Response |
| --- | --- |
| `Range: bytes=0-3` | 206, `Content-Range: bytes 0-3/10`, four bytes |
| `Range: bytes=4-` | 206, bytes 4 through 9 |
| `Range: bytes=-3` | 206, bytes 7 through 9 |
| `Range: bytes=0-99` | 206, bytes 0 through 9 |
| `Range: bytes=10-` | 416, `Content-Range: bytes */10`, no body |

File responses advertise `Accept-Ranges: bytes`. HEAD ignores Range and returns
the full representation's metadata. Cache-validation hits return 304 before
range selection. Empty files ignore Range and return an empty 200.

Unsupported units, malformed ranges, multiple ranges and repeated Range fields
fall back to a full response. Large decimal values are handled without integer
overflow. A valid range with no satisfiable bytes returns an empty 416.

The host currently has no strong validator for If-Range. Requests carrying it
fall back to a full response after other conditions are evaluated, allowing the
client to replace its old copy. Do not use the weak ETag to combine fragments
from different requests as verified resumable downloads. Strong content
validators and multipart ranges remain future work.

The body seeks directly to the selected offset and reads at most the selected
length, in chunks of at most 16 KiB. HEAD and successful validation do not read
file contents. Dropping the body releases its stream; no separate lease or
background reader is added. Files growing during transmission cannot append
unexpected bytes; premature EOF is reported as a body error.

Publish file changes using temporary files and atomic rename. Open file handles
preserve existing responses across replacement, but cannot protect against
in-place writes to the same inode.

## Internal interface

The host implementation adds one stateless cross-module function,
`static_files::response`:

```rust
pub(crate) fn response(
    file: std::fs::File,
    media_path: &str,
    request: &http::Request<axum::body::Body>,
) -> crate::Result<axum::response::Response>;
```

The caller validates methods, opens a confined regular file and invokes this
function on a blocking thread. The returned body owns that file. No state types,
members, SQL, WIT or public APIs are added; existing routing and file-opening
function signatures remain unchanged.
