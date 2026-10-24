# Issue read queries — feature design

**Status:** Authorized internal SQLite detail/list queries are implemented. Slice225 adds a [session-bound browser list endpoint](browser-issue-list-design.md); slice226 adds [bounded detail/history HTTP](browser-issue-detail-design.md). Slice253 adds the initial per-project text/exact-key search journey described below. MCP, structured filters/facets, cross-project search and saved views remain pending. The original internal-query gate used `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18498 make check`; newer evidence is in the development plan.

**Requirements:** `docs/development-plan.md`, sections 5–6 and 10.2–10.3.

## Actor and journeys

- An active project manager, member, or viewer can read issues only through explicit project membership. This includes guests with explicit project membership. Organization owner/admin and installation-admin status do not imply content access.
- A member opens the project issue list, pages through issues in a deterministic order, then opens one issue by its immutable project-local number. An authorized direct read can still open an issue in trash until purge; the default list excludes trashed issues.
- Archived projects remain readable to their explicit members, while their mutation policy is enforced by commands such as issue creation.
- Missing project, missing issue, inactive actor, or actor without read access have one internal `Unavailable` outcome. Future HTTP mapping uses 404 for unknown and inaccessible private resources to avoid existence disclosure.

## Consistent authorization and persistence

- Start a read transaction, load current access context, authorize `ReadProjectContent`, and query the issue/list on the same SQLite transaction. This pins a consistent database snapshot: a membership change cannot be observed between the access check and content read.
- `IssueStore` owns read-model queries. `SessionIssueReader` now binds the list query to bearer-derived identity and the same caller-owned read transaction. The remaining ID-based internal APIs require a trusted authenticated caller, never a client-selected principal.
- List items are summaries that omit descriptions. Detail queries may include the Markdown description and the current content-free functional history.

## Ordering and bounds

- List only issues with `trashed_at IS NULL`, ordered by immutable positive `issue_number` ascending. The keyset cursor is the last returned issue number; creation allocates strictly increasing numbers, so pages do not require OFFSET scans or mutable-rank cursors.
- Require page size 1–100 and fetch one extra row to determine whether another page exists. The returned continuation cursor is absent when there is no next page.
- Pagination is keyset-stable, not a multi-request snapshot: a concurrently created issue whose number exceeds the cursor may appear in a subsequent page. A client may restart from the beginning when it needs a fresh collection view.
- Issue listing does not implement arbitrary filters or board rank ordering. Search is a separate indexed, session-bound query; facets, saved views, cross-project search and structured filters remain separate work.

## Slice 253 — bounded project search

- The first search journey is explicit and project-scoped: `GET/HEAD /api/v1/projects/{project_id}/issues/search?q=...`. It uses the existing active-session and explicit project-membership boundary in the same SQLite read transaction. Organization/admin or installation-admin authority alone is insufficient. Each request rechecks current access; a cursor is navigation state, never an access grant.
- Index only issue title and description. Comments, history, fields, labels, trashed issues and other projects are not searchable through this route. A project-local issue key is matched exactly, case-insensitively; a key-shaped query does not fall back to text matching. Other queries are literal Unicode-whitespace-separated terms combined with AND, using the FTS5 `unicode61` tokenizer. User input is quoted into an FTS expression, so FTS operators are not a query language. No snippets, counts, relevance rank, facets, arbitrary filters, cross-project search or saved views are returned.
- Bound `q` to 512 UTF-8 bytes, 256 Unicode scalars and 16 non-empty whitespace-separated terms; reject blank/control-character input. Page size remains 1–100 with one lookahead. Results use `(issue_number, issue_id)` ascending keyset order and an opaque canonical Base64URL cursor; no OFFSET or relevance-ranked cursor. Concurrent edits can change membership between requests, so traversal is not a frozen snapshot.
- Keep FTS rowids separate from `issues.rowid`: issue IDs are text primary keys, and SQLite may change an implicit rowid during `VACUUM`. An integer-keyed search-document table owns stable FTS rowids and mirrors title/description transactionally via triggers; migration backfills existing issues. Search response items are existing summaries, with no description/snippet.
- The browser URL carries the search query and cursor. Opening a result records both in the detail URL so the return link restores the same results page. The search form submits an ordinary GET navigation; it does not persist private query/results in browser storage.
- Verification uses real migrated SQLite and the real Axum router. Cover migration backfill and insert/update/delete trigger consistency, literal FTS syntax, exact key, trash/project isolation, current session/membership roles and revocation, generic private 404, malformed/over-limit queries and cursors, lossless IDs/page boundaries, OpenAPI/HEAD/no-store, typed client validation, and Chromium search→detail→return plus private-data concealment. No provider or external service is involved.

## Verification plan

Use real migrated SQLite files. Verify manager/member/viewer reads, denial of organization-only and installation-only actors, banned/disabled denial, archived-project reads, direct lookup of a trashed issue, default list exclusion, deterministic page boundaries with concurrent creation, cross-project isolation, inaccessible/missing equivalence, and malformed page/cursor rejection. No provider or external service is involved.
