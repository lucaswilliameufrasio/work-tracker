# User session management — feature design

**Status:** Domain listing/revocation and opt-in authenticated current-session/individual/global-revoke HTTP are implemented; paginated session-list HTTP and UI remain pending.
**Requirements:** `docs/development-plan.md`, sections 7.2 and 19.2.

`SessionStore::list_active` returns only sessions owned by the supplied authenticated user that still pass the same active-account, timestamp, absolute-expiry, and inactivity checks used for token validation. It marks the current session by opaque session ID and returns creation, activity, and expiry timestamps. It never returns token digests or bearer values, and this schema does not collect IP address, user-agent, or device fingerprint metadata.

Expired, revoked, malformed, future-dated, and otherwise invalid rows are omitted rather than exposed as active. Session revocation is already owner-scoped; global revocation ends every session for the user. The HTTP adapter must derive the user/current session only from a validated cookie, never request input, and use generic not-found behavior for a foreign session ID.

`DELETE /api/v1/auth/session` (slice216) validates the exact cookie plus Origin/CSRF and derives the revoked user/session inside the same write transaction as audit. It clears the cookie only after commit and leaves other sessions intact. See [session logout](session-logout-design.md). Slice218 extends termination as documented below. The product still needs paginated session-list HTTP and the “My sessions” UI with current-session labeling and explicit confirmations. Whether to capture coarse device metadata is a product/privacy decision and is intentionally not guessed here.

## Individual/global revocation contract — 2026-10-19

Slice218 adds opt-in `DELETE /api/v1/auth/sessions/{session_id}` and `DELETE /api/v1/auth/sessions`. Both use the existing same-origin/CSRF transport boundary and revalidate the exact bearer/account/deadlines in the write transaction. Ownership comes only from that validation, never from request IDs. Individual targets must be canonical UUIDs; foreign, missing and already-revoked targets share a generic 404. Successful individual revocation returns 204 and clears the cookie only when the target is the current session. Global revocation includes the current session, returns 204 and clears the cookie only after commit. Both preserve other users' sessions and atomically append the existing content-free audit event; audit failures roll back all revocations. Concurrent global requests using the same bearer cannot both succeed after the first invalidates it.

These are self-service termination actions, not grants of privileged assurance; they do not require or create a reusable MFA proof. No query options or caller-selected user IDs are accepted. A valid session may revoke an expired but not-yet-revoked owned session. The ID-based internal APIs remain for existing trusted flows; HTTP must not call them outside the new session-bound operation. Session-list pagination and UI confirmations remain separate follow-up work, not fulfilled by these routes.
