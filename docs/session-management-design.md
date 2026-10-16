# User session management — feature design

**Status:** Domain listing/revocation and opt-in authenticated current-session/logout HTTP are implemented; session-list/individual/global-revoke HTTP and UI remain pending.
**Requirements:** `docs/development-plan.md`, sections 7.2 and 19.2.

`SessionStore::list_active` returns only sessions owned by the supplied authenticated user that still pass the same active-account, timestamp, absolute-expiry, and inactivity checks used for token validation. It marks the current session by opaque session ID and returns creation, activity, and expiry timestamps. It never returns token digests or bearer values, and this schema does not collect IP address, user-agent, or device fingerprint metadata.

Expired, revoked, malformed, future-dated, and otherwise invalid rows are omitted rather than exposed as active. Session revocation is already owner-scoped; global revocation ends every session for the user. The HTTP adapter must derive the user/current session only from a validated cookie, never request input, and use generic not-found behavior for a foreign session ID.

`DELETE /api/v1/auth/session` (slice216) validates the exact cookie plus Origin/CSRF and derives the revoked user/session inside the same write transaction as audit. It clears the cookie only after commit and leaves other sessions intact. See [session logout](session-logout-design.md). The product still needs session-list/individual/global-revoke routes and the “My sessions” UI with current-session labeling and explicit confirmations. Whether to capture coarse device metadata is a product/privacy decision and is intentionally not guessed here.
