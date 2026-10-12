# Issue creation — feature design

**Status:** The SQLite issue model and internal atomic creation command are implemented and verified by `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18495 make check`. No HTTP/MCP route, UI, due-date operation, idempotency record, or outbox worker is included in this slice.

**Requirements:** `docs/development-plan.md`, sections 2, 4–6, and 11.

## Actor and journey

- An active project manager or member may create an issue in a project they explicitly belong to. A viewer, organization role without project membership, installation-admin role, inactive account, and archived project do not grant issue mutation.
- The application boundary will supply the actor ID from a validated session. `IssueStore::create` is an internal use case, not an HTTP authorization boundary; it reloads current account, organization, project membership, and archive state inside `BEGIN IMMEDIATE` before authorization.
- The caller supplies a required title, optional Markdown description, and optional assignee. Type/status are not client-selectable in this command: the use case resolves the active `task` and `todo` system keys, independent of localized/editable names. Priority starts at `none`; due date is omitted until the organization-timezone rule is available.
- Optional assignee must be an active project manager/member, including a guest with explicit project membership. Viewers and non-members cannot be assigned.
- Under the same write transaction, allocate the next project issue number, choose a rank in the `todo` column, insert the issue, append a content-free `issue.created` history event, and commit. A failure at any step leaves no issue, history event, or consumed counter value.

## Data and invariants

- Issue IDs use UUIDv7. The public identifier is `<immutable project key>-<issue number>`; issue ID, project, number, and creator authorship are immutable. Allocation begins at 1 and increments transactionally. A uniqueness constraint prevents duplicate numbers, and no delete/purge path rewinds the counter.
- Composite foreign keys bind issue type, status, and assignee to the same project. The creator is retained as authorship. The database checks title length (1–240 Unicode scalar values), Markdown description byte size (at most 64 KiB), priority enum, positive issue number/version, and integer rank.
- Initial rank uses a 1,024-point stride after the current maximum rank in the active target column. This leaves integer gaps for later inserts; rebalance/move behavior remains a separate slice.
- History records operation, actor, issue, project, and UTC time only. It has no free-text diff/payload, so create history cannot duplicate the title or description.
- Project-local today and timezone-aware due-date validation are not approximated with UTC. This command does not accept a due date; adding that contract requires using the configured IANA organization timezone.

## Transaction and module ownership

- `src/access.rs` loads current authorization context and evaluates `MutateIssues`.
- `src/issues/` owns title/description validation, default type/status resolution, assignment eligibility, number/rank allocation, issue persistence, and its initial history event.
- SQLite migrations own same-project FKs, uniqueness, limits, and the project counter.
- The caller owns HTTP/MCP serialization, cookie/session/CSRF, response codes, and future idempotency-key handling. No outbox consumer or automation is installed; issue creation does not yet trigger notifications/automations.

## Deliberately deferred

Idempotency-Key semantics and retention, outbox event/job leases and delivery, due dates, arbitrary type/status selection, issue update/move/reopen/trash/purge, comments, FTS, routes/UI, and real-time delivery are not silently approximated. Before exposing creation through a retrying transport, implement its operation/principal-scoped idempotency contract. Before supporting due dates, parse and compare against the organization-local day. Before triggering automations, persist versioned outbox work in the same mutation transaction and implement durable claim/retry/recovery.

## Verification plan

Use real file-backed SQLite migrations. Cover allowed/denied actors, active/default catalog resolution by system key after renaming, Unicode title and byte-bounded description, assignment role/state, atomic counter/rank under concurrent creates, same-project FKs, archived/default-schema failure, and rollback when history persistence fails. No provider, client account, or live external system is required.
