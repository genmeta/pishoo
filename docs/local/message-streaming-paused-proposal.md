# Message streaming experiment — paused 2026-10-28

The user asked to save this experiment on a branch and return to the original
branches. No further redesign implementation or live deployment rollback is
part of this archival operation.

## Resumed 2026-11-05

The four original experimental branches were rebased onto their latest origin/main.
The implemented long-lived guest SSE design is restored for evaluation; the host-owned
alternative below remains unimplemented. Standard Message policy now accepts the exact
current official author and the legacy std.dhttp.net author, using the shared standard
author check. Tests and SQL follow the current owning-directory layout.

## Implemented snapshot

- Shared Engine/Linker and compiled Component; Store/Instance per invocation.
- WASI event hints current/notify/wait; long-lived guest SSE on GET message/list
  with explicit Accept, durable message change cursor, bounded state-patch cache.
- Fifteen-second transport heartbeats, without periodic database fallback queries.
- Workspace session contact/profile/avatar caching and removed directory timers.
- Completed contact applications no longer poll remote relationships.

This is a full working-tree snapshot, including pre-existing permission/UI work
that overlapped these files. Those pre-existing edits were separately preserved
as uncommitted changes on the original branches.

## Discussed alternative — NOT IMPLEMENTED

The host would own long-lived connections, heartbeats, cancellation and backpressure.
WASM would execute only finite business operations using shared compiled code and
per-invocation Store/Instance. Existing SDK/WIT need not gain methods if HTTP result
metadata and structured CLI stdout express a change marker. These representations
were only proposals; no final header name, result schema or ABI was approved.

A mutation must commit before reporting change. The host would process the marker
after execution finishes, conservatively wake on abnormal mutating execution, and
register subscription interest before querying to prevent a query/wait race. Reads
and empty background jobs must not emit changes. Business cursors and authorization
remain with Message; host invocations must preserve the real caller identity.

Database epoch plus sequence is an opaque replay cursor, independent of Wasmtime
epoch interruption. A resident instance per Lib was considered but not adopted:
concurrency, request identity and lifecycle would require a new service contract.

## Related snapshots

- message: `wip/message-streaming-20261028` at `bc4125156d19a904d3cc52954415eddf4528f315`; original `main`.
- workspace: `wip/message-streaming-20261028` at `c60880f582d726fe15a1cfb0d881353be913aaf9`; original `main`.
- gmutils: `wip/message-streaming-20261028` at `f593017ceee9263c96f90becf5e138e3ebd5859a`; original `release/0.9.0`.
