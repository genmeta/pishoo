# pishoo Workspace

This Solid/Vite application is the profile Workspace served by pishoo at
`/workspace/`. The shell exposes contacts, API extensions, apps, approvals and
quick settings. The existing contacts, approvals and advanced access-control
pages consume daccess APIs mounted by the same pishoo profile server. Outbound
contact requests use owner-only Workspace APIs. The backend also exposes
owner-only APIs for the public display name (`GET/PATCH /workspace-api/settings/profile`)
and avatar (`GET/PUT/DELETE /workspace-api/settings/profile/avatar`)
An empty display name clears it. The settings pages support editing the public display name; the home page shows
identity and pending reviews. The add-contact form keeps requested and offered
capabilities separate for each request; the sent-requests view supports manual
status checks and deletion of local records. Display names and avatars are
published through `GET /std/profile` and `GET /std/profile/avatar`. Established
contacts are displayed with those public fields through owner-only same-origin
proxy endpoints. Pishoo does not persist or proactively cache remote profiles;
browser HTTP caching handles reuse, and failures fall back to the shortened
identity name and initials.
The capability catalog describes profile-level capabilities. A contact request
contains `requested_capabilities` (what the sender wants from the recipient)
and `offered_capabilities` (what the sender offers to the recipient). The
backend maps those capability IDs to daccess rules. Granting an incoming
capability activates the contact and applies only that capability's fixed rules.
Recipient shorthand is normalized to a full `.dhttp.net` name by the backend;
both spellings address the same pending request. Workspace displays identity
labels without the `.dhttp.net` suffix, while requests, links and editable
access-rule values keep the canonical full name.
Extension/app pages remain clearly labeled as unavailable.

The contact directory shows usable, saved, and blocked identities. Incoming Chat
capability requests appear in the approval center, alongside access reviews;
there is no separate received-contact request list. Sent requests have their own
page until the remote profile accepts the requested capabilities.
Contact details link to advanced access rules scoped to that contact. Since
daccess currently has no status filter for `GET /contacts`, the UI temporarily
loads all backend pages before filtering and paginating locally; a server-side
filter will be needed for large directories.

## Development

Use Bun 1.4.2 or a compatible version that reads the checked-in lockfile.
Start an isolated pishoo profile backend, then run:

```sh
bun install --frozen-lockfile
PISHOO_WORKSPACE_BACKEND=http://127.0.0.1:3000 bun run dev
```

The development entry point is `http://localhost:5173/workspace/`. Production
builds are generated automatically by the pishoo Cargo build script.

## Verification

```sh
bun run typecheck
PISHOO_WORKSPACE_E2E_BACKEND=http://127.0.0.1:3100 bun run e2e
```

The E2E backend must be an isolated profile fixture and must not use a daily
development database. `e2e/navigation.spec.ts`, `e2e/settings.spec.ts` and
`e2e/contacts-directory.spec.ts` and `e2e/outbound.spec.ts` mock their API requests and can be run without
a live profile backend. The original dual-profile DHTTP/mTLS tests are preserved in
`pishoo/tests/deferred/workspace_network.rs`. Production outbound wiring is
pending the transport interface update; the current Rust application tests use
injected transports. See [the integration notes](../DACCESS.md) for this rebase's
scope and current limits.
