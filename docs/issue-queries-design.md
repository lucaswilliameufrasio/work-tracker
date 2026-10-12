# Issue read queries — feature design

**Status:** Authorized, internal SQLite detail and list queries are implemented and verified by `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18498 make check`; no HTTP/MCP endpoint or UI is part of this slice.

**Requirements:** `docs/development-plan.md`, sections 5–6 and 10.2–10.3.

## Actor and journeys

- An active project manager, member, or viewer can read issues only through explicit project membership. This includes guests with explicit project membership. Organization owner/admin and installation-admin status do not imply content access.
- A member opens the project issue list, pages through issues in a deterministic order, then opens one issue by its immutable project-local number. An authorized direct read can still open an issue in trash until purge; the default list excludes trashed issues.
- Archived projects remain readable to their explicit members, while their mutation policy is enforced by commands such as issue creation.
- Missing project, missing issue, inactive actor, or actor without read access have one internal `Unavailable` outcome. Future HTTP mapping uses 404 for unknown and inaccessible private resources to avoid existence disclosure.

## Consistent authorization and persistence

- Start a read transaction, load current access context, authorize `ReadProjectContent`, and query the issue/list on the same SQLite transaction. This pins a consistent database snapshot: a membership change cannot be observed between the access check and content read.
- `IssueStore` owns read-model queries. Project/session extraction remains the responsibility of a future authenticated route; callers supply a validated actor ID, never a client-selected principal.
- List items are summaries that omit descriptions. Detail queries may include the Markdown description and the current content-free functional history.

## Ordering and bounds

- List only issues with `trashed_at IS NULL`, ordered by immutable positive `issue_number` ascending. The keyset cursor is the last returned issue number; creation allocates strictly increasing numbers, so pages do not require OFFSET scans or mutable-rank cursors.
- Require page size 1–100 and fetch one extra row to determine whether another page exists. The returned continuation cursor is absent when there is no next page.
- Pagination is keyset-stable, not a multi-request snapshot: a concurrently created issue whose number exceeds the cursor may appear in a subsequent page. A client may restart from the beginning when it needs a fresh collection view.
- This query does not implement FTS, arbitrary filters, board rank ordering, or search facets. Those need their own indexed query contracts and must apply the same project authorization before returning rows or counts.

## Verification plan

Use real migrated SQLite files. Verify manager/member/viewer reads, denial of organization-only and installation-only actors, banned/disabled denial, archived-project reads, direct lookup of a trashed issue, default list exclusion, deterministic page boundaries with concurrent creation, cross-project isolation, inaccessible/missing equivalence, and malformed page/cursor rejection. No provider or external service is involved.
