# pishoo Workspace

This Solid/Vite application is the profile Workspace served by pishoo at
`/std/workspace/`. The shell exposes contacts, apps, approvals and quick settings.
The approval center is a primary navigation destination at `/std/workspace/approvals`,
with its own pending count. The existing contacts, approvals and advanced access-control
pages consume daccess APIs mounted by the same pishoo profile server. Outbound
contact requests use owner-only Workspace APIs. The backend also exposes
owner-only APIs for the public display name (`GET/PATCH /std/workspace-api/settings/profile`)
and avatar (`GET/PUT/DELETE /std/workspace-api/settings/profile/avatar`)
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
The apps page merges the loaded Sandbox catalog with the disk catalog by local
Lib ID. It shows running, saved/restart-required, removed/restart-required and
invalid disk resources separately. A declared `x-pishoo.entry` in the running
manifest enables Open in a new tab; legacy components keep their individual GET
API links in the Interfaces section. Details are full pages with bookmarkable
sections, back/forward navigation and preserved list filters.

Connected app APIs:

- `GET /std/workspace-api/libs` and `GET /std/pishoo/libs` return their own
  OpenAPI snapshots, including the optional frontend `LibSummary.openapi`.
- `POST /std/pishoo/lib-check` inspects/compiles a chosen file before confirmation;
  `PUT /std/pishoo/libs/<id>` saves it and `DELETE` removes the component.
  Both take effect after restart and preserve data and access rules.
- `GET /std/acl/access` provides the saved rules shared by visitor, feature and API
  views; `/std/contacts` provides paginated subject suggestions. PATCH/DELETE on
  `/std/acl/allow/<grantee>` submit individual changes with read-back after each
  batch. Partial failure, unknown results and unexecuted changes remain visible.
  Presets are derived from the running manifest; they never become saved roles.

The apps list has My apps and App store tabs, with one Check for updates action.
Store browsing, exact-version details and release history use
`/std/dhttp/store.dhttp.net/store/v1`. The live store currently has an empty
catalogue. Search, publisher filtering and pagination use its real response
shapes; errors remain distinct from empty results.

Update checks use saved complete IDs and numeric MAJOR.MINOR.PATCH versions,
send at most 100 applications per batch, and show results beneath My apps.
Current, update, not-found, unavailable and failed batches are shown separately.
Checks disclose that the store receives IDs, versions and the DHTTP caller
identity. Legacy short IDs are never assigned a publisher from their manifest.

Store installation fixes publisher, name and version at confirmation and calls
`POST /std/pishoo/store-install`. The native host uses the pinned store owner
hash from the deployment record, fetches the exact published release, verifies
the package size/digest, original manifest, historical signer proof and signature,
then compiles and atomically saves it. Browser proxy metadata is for browsing;
the browser does not download and re-upload a package as a trusted installation.
Saving preserves data and rules and requires restart. Installation never applies
suggested access rules.

Complete IDs are `<publisher>/<name>` and have separate lib/db directories and
API mounts. Old-resource migration, external template resources and version
precondition headers remain outside this implementation.
Local metadata and declared publisher/capabilities are not presented as signature
or import verification.

The contact directory shows usable, saved, and blocked identities. Incoming Chat
capability requests appear in the approval center, alongside access reviews;
there is no separate received-contact request list. Sent requests have their own
page until the remote profile accepts the requested capabilities.
Contact details link to advanced access rules scoped to that contact. Since
daccess currently has no status filter for `GET /std/contacts`, the UI temporarily
loads all backend pages before filtering and paginating locally; a server-side
filter will be needed for large directories.

## Development

Use Bun 1.4.2 or a compatible version that reads the checked-in lockfile.
Start an isolated pishoo profile backend, then run:

```sh
bun install --frozen-lockfile
PISHOO_WORKSPACE_BACKEND=http://127.0.0.1:3000 bun run dev
```

The development entry point is `http://localhost:5173/std/workspace/`. Production
builds are generated automatically by the pishoo Cargo build script.

## Verification

Shared Workspace controls live in `src/index.css`, with theme overrides in
`src/neo-brutal.css`. Reuse `section-nav` for page sections, categories and approval
status; `view-select` for grouping and sorting; `text-link` for quiet navigation;
and `button-group` for inline actions. Mutating actions use the existing
`button-primary`, `button-secondary` and danger variants. Page CSS should only
add layout constraints, not duplicate these controls' colors, borders or states.

Dropdowns keep native `<select>` semantics and use shared `base-select` picker
styles, including their open menu, in supporting browsers. The feature query
keeps a styled trigger and the system picker as a fallback in other browsers;
see [customizable select support](https://developer.mozilla.org/en-US/docs/Learn_web_development/Extensions/Forms/Customizable_select).

```sh
bun run typecheck
PISHOO_WORKSPACE_E2E_BACKEND=http://127.0.0.1:3100 bun run e2e
```

The E2E backend must be an isolated profile fixture and must not use a daily
development database. `e2e/navigation.spec.ts`, `e2e/settings.spec.ts` and
`e2e/contacts-directory.spec.ts` and `e2e/outbound.spec.ts` mock their API requests and can be run without
a live profile backend. Rust application and transport tests run from the repository root with
`cargo test --locked --workspace -- --test-threads=1`. Keep test identities and
databases separate from profiles used by the installed service.

## App-page acceptance (2026-10-13)

The isolated Playwright fixtures cover 1440×900, 1024×768, 768×1024 and
390×844. Screenshots include local cards, the connected store, full details,
all three access views, file install/update and uninstall confirmations, empty
catalogs, query failures and partial daccess writes, plus fixed store-version
confirmations and batch update errors. These browser fixtures demonstrate UI
behavior; Rust tests verify signature tampering, atomic saving, full-ID isolation,
owner continuity and protocol input rejection. Public anonymous and existing
Home proxy reads have independently verified the deployed empty catalogue.

```sh
bun run typecheck
bun run build
PISHOO_WORKSPACE_E2E_BACKEND=http://127.0.0.1:9 bun run e2e e2e/apps.spec.ts e2e/navigation.spec.ts e2e/settings.spec.ts --output=../../output/playwright/workspace
```

These three specs intercept their APIs and do not require a profile database.
Use the repository-required Bun version (1.4.2) for Cargo's frozen dependency
installation. Screenshots are generated in `output/playwright/workspace` at the
repository root; `output/playwright/workspace-review.html` indexes the acceptance
captures from this implementation run. Other E2E specs may need the isolated
backend described above.
