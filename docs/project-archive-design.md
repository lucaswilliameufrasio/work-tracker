# Project archive lifecycle — feature design

**Status:** Internal slice 210 implemented; no transport/UI.
**Source:** `docs/development-plan.md` §5–6.

An active project manager or organization owner/admin can archive and reopen a project. Installation-admin status alone does not authorize either transition. Organization authority never creates project-content access or membership. The caller supplies an authenticated actor ID; current account/roles are reloaded in `BEGIN IMMEDIATE` before every transition, including no-ops.

`ProjectStore::set_archived` takes a desired archived boolean, not a toggle. This avoids reversing state when a request is retried. A matching state returns unchanged without rewriting timestamps or appending audit. An actual transition changes only `archived_at`/`updated_at` and appends `project.archived` or `project.reopened` atomically; audit failure rolls back the state. Missing and inaccessible projects return the same unavailable error. Memberships, issue content, schemas and numbering are preserved. No project deletion is introduced.

Existing issue and membership services read current archive state, making content read-only while archived; authorized direct reads remain available. Reopening a project without an active manager does not bypass the existing managerless read-only guard. Automation rules/schedules do not exist in product persistence yet: this slice does not invent a pause flag or claim scheduler integration. Those future modules must pause atomically on archive, revalidate before execution, and require explicit reactivation after reopening without catch-up/replay.

Integration coverage must exercise role/account denial, repeated transitions, audit rollback, concurrent same-state requests, retained memberships/content/numbering, current read/write enforcement, migration preservation and audit constraints. HTTP/MCP/session-principal, sidebar/search exclusion and rules/schedules remain separate work.
