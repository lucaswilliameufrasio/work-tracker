# Restricted MFA recovery — feature design

**Status:** Slice 195 creates a restricted, short-lived recovery token; slice 196 can start one encrypted pending TOTP replacement using that token while retaining the active factor. Confirming/replacing TOTP and rotating the remaining recovery codes are still open. No transport or UI is included.
**Requirements:** `docs/development-plan.md`, sections 7.1–7.2 and 7.6.
**Related:** [TOTP profile and recovery codes](mfa-design.md), [credential lifecycle](mfa-credential-lifecycle-design.md), [session step-up](mfa-session-step-up-design.md).

## Approved recovery contract

For a non-installation-admin account that has an active password credential, confirmed email, active TOTP credential, and unused recovery code, beginning lost-device recovery requires the confirmed email, current password, and one recovery code. The password hash and active account/email/TOTP/admin state are rechecked in the same `BEGIN IMMEDIATE` transaction that consumes the code and creates the recovery record. A password change/reset racing password verification therefore prevents recovery from starting.

Successful authentication returns a random 256-bit opaque recovery token with a strict 10-minute lifetime. Only its domain-separated digest is persisted. It is a distinct type and database record, not a normal session, login bearer, `RecentMfaProof`, or capability for general account/admin actions. At most one unfinished recovery record is active per user; another start does not consume a code or replace an existing live recovery token. A new attempt may start after the old token expires or is explicitly invalidated.

Wrong password, invalid/missing recovery code, inactive or ineligible account, missing active TOTP, installation-admin status, and an already-open recovery flow have the same rejected outcome. Password/code attempts use the existing per-IP, normalized-identity, and installation rate limits before password KDF work. Public adapters must still enforce request limits, trusted client-IP resolution, CSRF/Origin, and generic response handling.

The code is consumed when a recovery token is successfully issued. If the user abandons the flow, that code stays consumed, the existing active TOTP remains unchanged, and other unused recovery codes remain available. Slice 196 adds one pending replacement enrollment per recovery token, with an encrypted 65-byte TOTP envelope and a deadline no later than the recovery token's expiry. The seed is returned in zeroizing memory; a second begin attempt does not replace or redisplay it. The active credential remains untouched. Subsequent recovery-only operations may confirm the replacement and rotate the recovery-code batch; they must not change email/password, create or elevate a normal session, or authorize privileged actions. Every mutating operation must revalidate the exact recovery token inside its own write transaction; a preflight `validate()` result is not authority. Completing replacement must invalidate the recovery token, clear prior session MFA assurance, and require a fresh ordinary password+TOTP login. The replacement enrollment is confirmed before the old active credential is deleted.

Installation admins are rejected by this public/account recovery flow and receive no MFA assurance from recovery codes. The development plan calls for a separate host/CLI administrative recovery path using local access and recovery material; that path remains unimplemented and is not a password-only bypass. The portable recovery kit remains blocked by ADR 0002.

## Alternatives and consequences

- A recovery code as a normal-login substitute was rejected because it would create a general session during credential reset and blur recovery authority with MFA assurance.
- Code-only public recovery was rejected; requiring the current password makes theft of a recovery code alone insufficient to replace a factor.
- Deleting the old TOTP when recovery begins was rejected. The active credential remains valid until the user proves possession of the replacement TOTP, so an abandoned or interrupted flow does not disable the old factor.
- The 10-minute lifetime aligns with the plan's initial recent-authentication window. Revisit it if measured recovery usability or abuse evidence warrants a different bounded lifetime; do not extend it on use.

Recovery start is internal only. Login challenge integration, recovery HTTP/MCP routes, cookies, UI, CAPTCHA/rate-limit transport wiring, setup-time code presentation, installation-admin CLI recovery, and the portable kit remain separate work.
