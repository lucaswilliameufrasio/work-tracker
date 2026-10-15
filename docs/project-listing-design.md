# Accessible project listing — feature design

**Status:** Internal slice 211 implemented; no HTTP/MCP/UI.
**Source:** `docs/development-plan.md` §5–6, sidebar/navigation and access rules.

The navigation journey lists projects an active organization member can actually read. Explicit project membership is required, including for owner/admin and installation-admin accounts. Guest memberships are eligible; an organization member with no projects gets an empty page. A missing, inactive or unaffiliated actor is rejected. Archive state and memberships are re-read on every call, never cached as long-lived claims. Slice 212 adds a separate [operational catalog](project-operational-catalog-design.md) without broadening navigation access.

`ProjectStore::list_accessible` runs authorization and selection within one SQLite read transaction. It returns only project ID, stable key, name, caller's project role and optional archive timestamp; it never returns descriptions, issues or counts from inaccessible projects. Each bounded result is checked through the existing `ReadProjectContent` policy. Archived projects are excluded by default; explicit `include_archived` includes only archives the actor can read, not all organization projects. The operational owner/admin catalog is a separate query, not an alternate authorization mode on sidebar listing.

Pagination uses the immutable unique project key ascending, with page size 1–100, default 50, and one-row lookahead. The optional cursor is the last key; it need not still be accessible. It cannot widen access or change the current archive filter. Across requests this is not a frozen snapshot: later grants/creations may appear after the cursor, while revocations/archive changes take effect on the next call. Invalid page sizes/cursors or malformed persisted fields fail explicitly rather than silently widening results.

No schema or audit changes are needed for this read-only operation. Tests cover explicit roles, guest and admin boundaries, archived default/opt-in, current account/membership changes, bounded pagination and invalid/corrupt input. Principal/session binding, actual navigation rendering and search remain future work; the operational catalog is a separate slice.
