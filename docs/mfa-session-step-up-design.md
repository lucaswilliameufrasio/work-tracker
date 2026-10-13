# TOTP session step-up — feature design

**Status:** Internal session rotation and MFA assurance implemented in slice 193; transport and route integration remain open.
**Requirements:** `docs/development-plan.md`, sections 4, 7.1–7.2, 7.4, and 8.
**Related:** [ADR 0003 — TOTP secret encryption profile](adr/0003-totp-secret-encryption.md), [TOTP enrollment](mfa-enrollment-design.md), [reauthentication](reauthentication-design.md).

## Actor and journey

An active user with an active TOTP credential proves possession for the exact current opaque session. The internal step-up service accepts only the bearer token and OTP; it derives the principal from the persisted session, never from a caller-supplied user ID. A future HTTP boundary must authenticate the request, rate-limit before verification, enforce CSRF/Origin, and replace the session cookie with the rotated token after success.

1. Begin an immediate SQLite write transaction and load the exact token digest, active account, session lifetime, and revocation state.
2. Verify a TOTP step newer than the credential's last accepted step.
3. Revoke the old session and insert a new session ID/token in the same transaction, marking the new session's `mfa_authenticated_at`.
4. Preserve the old session's original creation time and absolute expiry. The new `expires_at` is the earliest of the old persisted deadline, original absolute deadline, and new inactivity deadline; elevation never extends the absolute session lifetime.
5. Append the TOTP verification and session-elevation audit events and commit atomically. Return the new opaque bearer only after commit.

Wrong/missing OTP, missing/expired/revoked session, inactive account, or missing active TOTP credential leaves the old session and TOTP counter unchanged. Failed TOTP attempts must be rate-limited by the future caller. If any mutation or audit append fails, the entire transaction rolls back.

## Assurance proof

The new session stores `mfa_authenticated_at` separately from password `reauthenticated_at`; password assurance is never reinterpreted as MFA. A `RecentMfaProof` is opaque and non-serializable, bound to the new user, session ID, bearer digest, exact persisted MFA timestamp, and conservative session-valid-until time. Its initial privileged window is 10 minutes, using the existing inclusive `age <= 10 minutes` convention; it cannot outlive the session. A future request may derive a proof only by presenting the same live bearer and loading its persisted MFA marker. Every protected mutation must recheck the exact session, account, and marker in its own transaction.

Session ID/token rotation is required by the product plan after elevation. The old token becomes invalid at commit; a transport that fails to deliver/set the new token will require the user to authenticate again. Do not copy password `reauthenticated_at` to the new row. Revocation/logout/ban continue to invalidate the rotated session through the existing session store.

## Boundaries

This internal slice does not add HTTP/MCP routes, cookies, CSRF/Origin controls, rate-limit wiring, provider step-up, recovery, credential disable/revocation, MFA policy settings, admin UI, or installation-admin authorization by itself. A recent proof is evidence of a factor in one live session, not a capability; each action still needs fresh account/role authorization. Installation-admin operations stay fail-closed until a caller explicitly requires `RecentMfaProof` and checks the action's authority.

## Implementation evidence

Migration `0017_session_mfa_assurance.sql` adds the independent nullable session marker and extends the immutable audit allowlist while preserving prior rows. `MfaStepUpService` loads identity only from the active session bearer, verifies/consumes TOTP and rotates the exact session under one `BEGIN IMMEDIATE`, preserving the absolute deadline and not copying password reauthentication. `RecentMfaProof` is non-serializable, tied to the new token digest/session/user and persisted marker; email mutations recheck it in their transaction. Five new integration cases cover admin email add/primary/remove, persisted proof expiry/revocation, concurrent elevation, absolute expiry, rollback, and migration preservation. These internal APIs do not close the product transport or broader admin-policy gates.
