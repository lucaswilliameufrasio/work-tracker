# Session-bound browser logout — feature design

**Status:** Slice 216 implemented, under the explicit browser-origin opt-in.
**Source:** `docs/development-plan.md` §7.2; browser/session principal contracts from slices213–215.

The signed-in user ends only the session identified by their exact cookie at `DELETE /api/v1/auth/session`. No user/session identifier is accepted as query input. Cookie/Host/Origin/Fetch Metadata and the session-bound `X-CSRF-Token` are checked before DB work. A missing/invalid CSRF or foreign/missing Origin cannot revoke or touch activity. Other browser endpoints remain read-only and login/signup stay closed.

`SessionStore::revoke_current` accepts a token and clock, opens `BEGIN IMMEDIATE`, revalidates its session/account/expiry, derives actor and session IDs from that record, and appends `auth.session.revoked` in the same transaction as revocation. No cached principal or request-selected ID authorizes the mutation. A concurrently revoked/expired/banned session fails authentication. Two concurrent logout attempts produce one revocation/audit; a retry after logout is unauthenticated, not a duplicate event.

Only after a successful commit does the handler return204/no-store and clear `__Host-puxiri_session` with the same host-only Path=/, Secure, HttpOnly, SameSite=Lax attributes and Max-Age=0. Failure of persistence/audit rolls back revocation and does not send clear-cookie; current valid-session requests can retry with the same CSRF value. The existing authentication preflight may perform its separately throttled activity touch; this is not part of the revocation transaction or privileged assurance.

Other sessions are not revoked, MFA factors/recovery codes are not changed, and no sessions/tokens are restored. Tests use the real router and SQLite to cover CSRF/Origin failures with no effect, current-session isolation, success/clear-cookie, concurrent/repeated logout, and audit rollback/retry. This is not global logout, a session-list UI, TLS-browser behavior, or authorization for account lifecycle actions.
