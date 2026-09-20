# pishoo Workspace

This Solid/Vite application is the profile Workspace served by pishoo at
`/workspace/`. Its access-control pages consume the daccess APIs mounted by the
same pishoo profile server.

## Development

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
development database.
