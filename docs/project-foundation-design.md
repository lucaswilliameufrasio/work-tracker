# Project foundation — feature design

**Status:** Project persistence, transactional creation, initial issue-schema catalogs, and an internal transactional issue-creation use case are implemented in separate slices; slice 186 passed `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18491 make check`, slice 188 passed `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18493 make check`, and slice 189 passed `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18495 make check`. Routes, issue listing/mutation, UI, and project browsing remain outside these foundations.

**Requirements:** `docs/development-plan.md`, sections 5–6 and 13.

## Actors and creation journey

- An active organization owner or admin may create a project. Installation-admin status alone, organization members/guests, and project roles do not grant this capability.
- The authenticated application boundary will supply the actor user ID. The project command re-reads current account and organization roles inside its own write transaction; session authentication and HTTP/MCP routes remain outside this foundation.
- The actor submits a 2–6 character uppercase ASCII project key, a non-empty name, and an optional description. The operation validates the input, starts `BEGIN IMMEDIATE`, checks current authorization, then atomically inserts the project, creator's sole initial `manager` membership, localized task type, five initial statuses, and a minimal `project.created` audit event before commit.
- A duplicate key, invalid actor, denied role, constraint violation, or audit failure must not leave a partial project, membership, or default catalog. Successful creation starts unarchived, with issue numbering at 1 and the board as the default view.
- The initial task type and statuses use immutable machine keys (`task`, `backlog`, `todo`, `in_progress`, `done`, `canceled`) separate from user-editable, locale-seeded names. The status categories/order/board visibility follow section 5.1 of the development plan. Issue-type icon/color keys are nullable because no initial visual tokens are specified; a future UI must render a safe fallback and allow only its own approved token set.

The key is unique within its organization and immutable by both omission of an update operation and a database trigger. Names are stored trimmed and must be non-empty. The optional description is stored as supplied. Length/content policies not fixed by the development plan are intentionally not invented here. The persisted default-view enum is `board`/`list`, defaulting to `board`; these are the two project-level presentation modes reflected in the planned board and list journeys.

## Data ownership and invariants

- `projects` owns stable identity/key, name/description, default view, archive timestamp, and next issue number.
- `project_members` owns explicit `manager`/`member`/`viewer` access. A composite foreign key requires organization membership; database triggers prevent a guest from being a manager and prevent changing an existing manager's organization role to guest.
- Creation derives project membership from the creator's successful project-creation authorization, not from organization role as an implicit content grant. The transaction rechecks persisted state under SQLite's writer lock to avoid authorizing from a stale pre-transaction context.
- Audit records contain only the actor, operation, target project ID, outcome, and timestamp; no project name, key, description, or content is copied into the audit row.
- Projects are archived rather than permanently deleted. Slice 210 adds the internal transactional archive/reopen operation described in [project archive lifecycle](project-archive-design.md); it does not provide a usable Kanban workflow or resume automations.

## Modules and contract

- `src/access.rs` owns current account/org-role loading and capability policy.
- `src/projects/` owns project creation, project input validation, and persistence.
- `src/projects/queries.rs` owns the bounded accessible-project listing for navigation; its explicit-membership contract is described in [project listing](project-listing-design.md). Organization/installation administration never broadens this list.
- `issue_types` and `statuses` are project-owned configuration catalogs; project creation seeds them in the same transaction as project identity, manager membership, and audit.
- `src/audit.rs` owns the allowlisted event kind/target and appends it to the caller-owned transaction.
- SQLite migrations own the project/membership constraints and the backward-compatible extension of the existing audit allowlists.

The public command returns a project ID and validated project fields on success. Expected failures distinguish invalid key/name, unknown actor, forbidden actor, duplicate key, corrupt persisted access state, and storage/audit errors. The command does not accept caller-selected membership roles, organization IDs, archived state, default view, or issue counter.

## Delivery boundary

Integration tests use the real SQLite file/migrations and verify the complete project transaction, persisted membership/default catalogs/audit, locale-specific names, denial and rollback paths, duplicate-key concurrency, guest constraints, and preservation of pre-existing audit rows and projects during migrations. Issue reads, invitations, membership management and archive/reopen are implemented in subsequent internal slices; routes, UI, issue updates, catalog editing, Kanban and idempotency keys remain pending. Issue creation has its own contract in `docs/issue-creation-design.md`; archive/reopen is covered by `tests/project_archive.rs`.
