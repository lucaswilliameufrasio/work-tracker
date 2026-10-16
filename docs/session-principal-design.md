# Session-derived principal — feature design

**Status:** Slice 213 implemented; session-bound project reads, no transport.
**Source:** `docs/development-plan.md` §6–7: identity comes from the current session and every operation checks current account/access.

The future browser adapter receives an opaque cookie, never an actor ID from request input. `SessionStore::authenticate_in_transaction` resolves its digest to an active, unrevoked, unexpired session within the caller-owned SQLite transaction. It returns a `SessionPrincipal` with private fields and read-only ID getters; arbitrary IDs cannot construct that type. It carries neither roles nor privileged/MFA proof and is not serializable. It is a point-in-time identity snapshot, not a reusable authorization grant.

`SessionProjectReader` owns one pool and session policy. Both accessible listing and operational catalog accept a session token, list options and clock, not actor IDs. Each starts one read transaction, derives principal and executes current access checks and data queries in that same snapshot. Revocations/bans/role changes committed before that snapshot apply; a later concurrent change does not retroactively invalidate an already-authorized read. There is no cross-request authorization cache.

Read-only transactional authentication does not update activity, extend expiry, assert recent reauthentication or rotate tokens. The eventual request boundary must perform the existing throttled `SessionStore::validate` activity check and then use transaction-bound operations, not pass its user ID into an unbound query. Mutations will need session revalidation within their own `BEGIN IMMEDIATE` transaction; this slice does not claim to secure arbitrary ID-based internal commands.

This reuses existing session policy/schema rather than creating a second identity store. Public internal ID-based queries remain for trusted non-HTTP callers but are not request boundaries. Tests must cover real session identity, isolated users, revocation/ban/expiry/future/corrupt timestamps, same-transaction uncommitted changes, unchanged activity, role changes and database failures. HTTP cookies/Origin/CSRF, login/setup/MFA enrollment and UI remain separate gates.
