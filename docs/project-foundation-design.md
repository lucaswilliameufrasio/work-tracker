# Project foundation — feature design

**Status:** Project persistence and transactional creation are implemented and verified by `RUSTUP_TOOLCHAIN=1.99.0 PUXIRI_E2E_PORT=18491 make check`. Issue types, statuses, issues, routes, UI, and project browsing are not part of this slice.

**Requirements:** `docs/development-plan.md`, sections 5–6 and 13.

## Actors and creation journey

- An active organization owner or admin may create a project. Installation-admin status alone, organization members/guests, and project roles do not grant this capability.
- The authenticated application boundary will supply the actor user ID. The project command re-reads current account and organization roles inside its own write transaction; session authentication and HTTP/MCP routes remain outside this foundation.
- The actor submits a 2–6 character uppercase ASCII project key, a non-empty name, and an optional description. The operation validates the input, starts `BEGIN IMMEDIATE`, checks current authorization, then atomically inserts the project, inserts the creator's sole initial `manager` membership, appends a minimal `project.created` audit event, and commits.
- A duplicate key, invalid actor, denied role, constraint violation, or audit failure must not leave a partial project or membership. Successful creation starts unarchived, with issue numbering at 1 and the board as the default view.

The key is unique within its organization and immutable by both omission of an update operation and a database trigger. Names are stored trimmed and must be non-empty. The optional description is stored as supplied. Length/content policies not fixed by the development plan are intentionally not invented here. The persisted default-view enum is `board`/`list`, defaulting to `board`; these are the two project-level presentation modes reflected in the planned board and list journeys.

## Data ownership and invariants

- `projects` owns stable identity/key, name/description, default view, archive timestamp, and next issue number.
- `project_members` owns explicit `manager`/`member`/`viewer` access. A composite foreign key requires organization membership; database triggers prevent a guest from being a manager and prevent changing an existing manager's organization role to guest.
- Creation derives project membership from the creator's successful project-creation authorization, not from organization role as an implicit content grant. The transaction rechecks persisted state under SQLite's writer lock to avoid authorizing from a stale pre-transaction context.
- Audit records contain only the actor, operation, target project ID, outcome, and timestamp; no project name, key, description, or content is copied into the audit row.
- Projects are archived rather than permanently deleted. This slice provides no archive mutation and does not yet create the issue types/statuses required for a usable Kanban workflow.

## Modules and contract

- `src/access.rs` owns current account/org-role loading and capability policy.
- `src/projects/` owns project creation, project input validation, and persistence.
- `src/audit.rs` owns the allowlisted event kind/target and appends it to the caller-owned transaction.
- SQLite migrations own the project/membership constraints and the backward-compatible extension of the existing audit allowlists.

The public command returns a project ID and validated project fields on success. Expected failures distinguish invalid key/name, unknown actor, forbidden actor, duplicate key, corrupt persisted access state, and storage/audit errors. The command does not accept caller-selected membership roles, organization IDs, archived state, default view, or issue counter.

## Delivery boundary

Integration tests use the real SQLite file/migrations and verify the complete transaction, persisted membership/audit, denial and rollback paths, duplicate-key concurrency, guest constraints, and preservation of pre-existing audit rows when the allowlist migration runs. Routes, UI, invitations, assigning other members/managers, archive/restore, issue templates, Kanban, idempotency keys, and last-manager/owner administration remain later slices.
