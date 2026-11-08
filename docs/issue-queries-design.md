# Issue read queries — feature design

**Status:** Authorized internal SQLite detail/list queries are implemented. Slice225 adds a [session-bound browser list endpoint](browser-issue-list-design.md); slice226 adds [bounded detail/history HTTP](browser-issue-detail-design.md). Slice253 adds the initial per-project text/exact-key search journey, slice254 adds its priority filter, slice255 adds an exact civil due-date filter, slice256 adds an exact issue-type filter, slice257 adds an exact status filter, and slice258 adds an exact assignee filter. Other structured filters/facets, MCP, cross-project search and saved views remain pending. The original internal-query gate used `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18498 make check`; newer evidence is in the development plan.

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
- Issue listing does not implement arbitrary filters or board rank ordering. Search is a separate indexed, session-bound query; slice254 adds priority, slice255 exact due date, slice256 exact issue type, slice257 exact status and slice258 exact assignee. Facets, saved views, cross-project search and other structured filters remain separate work.

## Slice 253 — bounded project search

- The first search journey is explicit and project-scoped: `GET/HEAD /api/v1/projects/{project_id}/issues/search?q=...`. It uses the existing active-session and explicit project-membership boundary in the same SQLite read transaction. Organization/admin or installation-admin authority alone is insufficient. Each request rechecks current access; a cursor is navigation state, never an access grant.
- Index only issue title and description. Comments, history, fields, labels, trashed issues and other projects are not searchable through this route. A project-local issue key is matched exactly, case-insensitively; a key-shaped query does not fall back to text matching. Other queries are literal Unicode-whitespace-separated terms combined with AND, using the FTS5 `unicode61` tokenizer. User input is quoted into an FTS expression, so FTS operators are not a query language. No snippets, counts, relevance rank, facets, arbitrary filters, cross-project search or saved views are returned.
- Bound `q` to 512 UTF-8 bytes, 256 Unicode scalars and 16 non-empty whitespace-separated terms; reject blank/control-character input. Page size remains 1–100 with one lookahead. Results use `(issue_number, issue_id)` ascending keyset order and an opaque canonical Base64URL cursor; no OFFSET or relevance-ranked cursor. Concurrent edits can change membership between requests, so traversal is not a frozen snapshot.
- Keep FTS rowids separate from `issues.rowid`: issue IDs are text primary keys, and SQLite may change an implicit rowid during `VACUUM`. An integer-keyed search-document table owns stable FTS rowids and mirrors title/description transactionally via triggers; migration backfills existing issues. Search response items are existing summaries, with no description/snippet.
- The browser URL carries the search query and cursor. Opening a result records both in the detail URL so the return link restores the same results page. The search form submits an ordinary GET navigation; it does not persist private query/results in browser storage.
- Verification uses real migrated SQLite and the real Axum router. Cover migration backfill and insert/update/delete trigger consistency, literal FTS syntax, exact key, trash/project isolation, current session/membership roles and revocation, generic private 404, malformed/over-limit queries and cursors, lossless IDs/page boundaries, OpenAPI/HEAD/no-store, typed client validation, and Chromium search→detail→return plus private-data concealment. No provider or external service is involved.

## Slice 254 — structured priority filter

- Extend the project search endpoint with an optional exact `priority` value (`none`, `low`, `medium`, `high`, or `urgent`). It combines with literal text search or an exact project-local issue key using AND; priority alone is also a valid search. At least one of `q` or `priority` is required. If `q` is supplied it retains slice253's bounds and validation, including rejection of blank/control-containing input.
- Apply the filter inside the same session-bound SQLite transaction and explicit project-membership authorization as the search. Keep trash exclusion, project scope, bounded page size, summary-only output, stable `(issue_number, issue_id)` order and cursor behavior unchanged. Priority-only pages use the `(project_id, priority, trashed_at, issue_number, id)` index added by migration0040; combined FTS queries apply priority as an additional predicate.
- The ordinary GET form exposes a native priority selector and allows an empty text box when a priority is selected. The URL preserves text, priority and cursor; detail return navigation restores all active search criteria. Clearing all search criteria returns to the ordinary issue list.
- Verify all five accepted values and invalid values at the HTTP boundary, priority-only and combined text/exact-key matches, mismatched criteria, trash exclusion, keyset pagination and query-plan index use. Also verify OpenAPI, TypeScript response/input validation and navigation, and Chromium form submission plus search→detail→return preservation. Do not claim other filters, facets or cross-project search.

## Slice 255 — exact civil due-date filter

- Extend the same project-scoped search with an optional canonical, real calendar date `due_date=YYYY-MM-DD`. Matching is exact equality on the stored civil date; it is not a range, an overdue predicate, or a filter for issues without a due date. It may be combined with literal `q` and/or exact priority using AND, or used alone. At least one criterion remains required; a supplied blank/invalid `q` stays invalid.
- Parse with the shared due-date validator and filter in the existing session/membership-authorized SQLite snapshot. Add a partial `(project_id, due_date, trashed_at, issue_number, id)` index for rows that have a date. Preserve trash/project scope, archived-project reads, summary-only output, page/cursor bounds and ordering.
- The GET form adds a native date input. The canonical date travels in the search URL, next-page link and detail-return URL; clearing the criteria returns to the ordinary list. No date arithmetic or timezone conversion is applied to exact matching.
- Verify invalid/noncanonical/impossible dates, due-date-only and all-criteria AND matches, mismatch/no-date/trash exclusion, keyset pagination, query-plan index use, OpenAPI, client response/input validation and Chromium submission/return preservation. This slice does not add ranges, overdue/missing-date options, other filters, facets or cross-project search.

## Slice 256 — exact issue-type filter

- Extend the same project search with optional `issue_type_id`, a canonical UUID of an issue type that belongs to that project. It combines with literal `q`, exact priority and exact `due_date` using AND, and it is sufficient alone. At least one criterion remains required.
- An archived type stays filterable. This applies the existing rule that an archived field or label with retained values remains a valid filter, and that archived labels leave the selector while remaining where they are used. A missing, foreign-project or malformed type is invalid: the request is rejected and does not become an empty or widened success page. Authorization still runs first, so an inaccessible project stays a generic 404 and does not reveal whether the type exists.
- The selector lists active types from the first catalog page (100, UUID order, `include_archived=false`) and does not walk further pages. When the current URL already filters by a type outside that page, including an archived type, that one record is loaded and shown so the active criterion stays visible and clearable. Other archived types stay out of the selector. The API still accepts an explicit archived type id.
- Add `(project_id, issue_type_id, trashed_at, issue_number, id)`. Preserve trash exclusion, project scope, summary output, page/cursor bounds and ordering. This slice does not add status, assignee, label, custom-field, range, facet, cross-project or saved-view search.

## Slice 257 — exact status filter

- Extend the same project search with optional `status_id`, a canonical UUID of a status that belongs to that project. It combines with literal `q`, exact priority, exact `due_date` and exact `issue_type_id` using AND, and it is sufficient alone. At least one criterion remains required.
- An archived status stays filterable under the same existing rule used for an archived type. A missing, foreign-project or malformed status is invalid and does not become an empty or widened success page. Authorization still runs before the existence check, so an inaccessible project stays a generic 404.
- The selector lists active statuses from the first catalog page (100, UUID order, `include_archived=false`) and does not walk further pages. When the current URL already filters by a status outside that page, including an archived status, that one record is loaded and shown. Other archived statuses stay out of the selector.
- Add `(project_id, status_id, trashed_at, issue_number, id)` for search order. The existing board index remains `(project_id, status_id, trashed_at, rank, id)`. This slice does not add assignee, label, custom-field, range, facet, cross-project or saved-view search.

## Slice 258 — exact assignee filter

- Extend the same project search with optional `assignee_user_id`, a canonical UUID of a current project member. It combines with literal `q`, exact priority, exact `due_date`, exact `issue_type_id` and exact `status_id` using AND, and it is sufficient alone. At least one criterion remains required. This is not a filter for issues without an assignee.
- Validity follows the assignment already stored on the issue. `issues.assignee_user_id` references `project_members`, so a retained assignment still belongs to a project member. Losing assignment eligibility (viewer role or inactive account) does not remove that membership and does not clear the assignment, so that member stays filterable. A member with no assigned issues is a valid filter and returns an empty page. A missing, foreign-project or malformed user is invalid and does not become an empty or widened success page. Authorization still runs before the membership check, so an inaccessible project stays a generic 404.
- The selector lists assignable members from the first roster page (100, UUID order) and does not walk further pages. Viewers and other ineligible members stay out of that list. When the current URL already filters by a member outside that page, or by a member who is no longer assignable, that one roster record is loaded and labeled `{nome} (não elegível)`. The API still accepts the explicit member id.
- Add a partial `(project_id, assignee_user_id, trashed_at, issue_number, id)` index for rows that have an assignee. The existing assignment index remains `(project_id, assignee_user_id, status_id, trashed_at, rank, id)`. This slice does not add an unassigned filter, label, custom-field, range, facet, cross-project or saved-view search.

## Verification plan

Use real migrated SQLite files. Verify manager/member/viewer reads, denial of organization-only and installation-only actors, banned/disabled denial, archived-project reads, direct lookup of a trashed issue, default list exclusion, deterministic page boundaries with concurrent creation, cross-project isolation, inaccessible/missing equivalence, and malformed page/cursor rejection. No provider or external service is involved.
