# MFA TOTP enrollment and credential storage

- **Status:** Internal persistence/enrollment implemented in slice 192; session-bound assurance/rotation is implemented separately in slice 193. Transport and privileged policy enforcement remain open.
- **Date:** 2026-10-13
- **Related:** [ADR 0003 — TOTP secret encryption profile](adr/0003-totp-secret-encryption.md), `docs/development-plan.md` sections 7.1–7.2

## Boundary and actor

The actor is an active user configuring an authenticator for their own account. Slice 192 supplies internal SQLite persistence services only. A future HTTP caller must establish the authenticated session, current account state, CSRF/Origin policy, rate limits, and authorization before calling them. An arbitrary user UUID is not proof of identity. No setup wizard, browser flow, enrollment route, QR/otpauth URL, or user-facing assurance UI is added. Session-bound assurance is the separate internal capability in `docs/mfa-session-step-up-design.md`.

## Credential lifecycle

`mfa_credentials` holds at most one row per user and has two valid states:

| State | Stored representation | Allowed transition |
|---|---|---|
| No credential | No row | Active user begins enrollment → pending |
| Pending | 65-byte ADR 0003 encrypted envelope; `activated_at` and replay counter absent; expiry present | Correct TOTP before expiry → active; expiry → delete; begin again → atomically replace pending secret/id |
| Active | Encrypted envelope; activation time and last accepted TOTP step present; pending expiry absent | Verify a newer TOTP step → advance replay counter; optional credential disable → separately authorized lifecycle in `docs/mfa-credential-lifecycle-design.md`; installation-admin disable is rejected |

The pending secret expires exactly 10 minutes after creation. Confirmation is valid only while `expires_at > now`; the boundary instant is expired. A wrong code does not consume or extend the pending enrollment. An expired row is removed when confirmation observes it or a new enrollment replaces it; no background cleanup job is part of this slice.

Beginning enrollment is allowed only for an active account without an active TOTP credential. Repeating it replaces any pending enrollment, making older displayed secrets unusable. The generated Base32 secret is returned once in a zeroizing value; callers must present it only through an authenticated setup response, and must never log, persist, or place it in a URL. This slice does not produce a QR code or `otpauth://` URL because issuer/display naming and transport are not defined.

Confirmation checks the RFC 6238 default profile (30-second steps, six digits, ±1 step), persists the matched step as already consumed, activates the credential, and appends a content-free audit event in one `BEGIN IMMEDIATE` transaction. The same code therefore cannot immediately be replayed after enrollment. Later verification advances the counter only for a strictly newer matching step; callers must combine that operation with a protected effect in the same write transaction when they need atomic step-up.

## Persistence and privacy

- Secret bytes are persisted only as the versioned encrypted envelope from ADR 0003. No clear seed, Base32 value, OTP code, email, or URI is written to SQLite or audit.
- User and credential UUIDs bind the derived key and AAD, preventing ciphertext row swaps.
- Pending replacement, activation, replay-counter advance, and corresponding audit events are atomic; audit failure rolls back the protected credential change.
- Account state is reloaded in the write transaction. Unknown/inactive accounts fail closed.
- A database-only copy does not reveal a seed, but a running process, host, or master key compromise remains in scope. SQLite deletion is not a claim of forensic erasure.

## Explicit exclusions

Optional active-TOTP disable exists as a separate internal operation documented in `docs/mfa-credential-lifecycle-design.md`; other credential revocation/replacement and recovery-code access remain absent. There is no MFA-required login challenge, HTTP/MCP route, frontend, CSRF/Origin or rate-limit wiring, audit viewer, automatic expiry sweeper, master-key rotation, or general admin-policy enforcement. The transaction-level credential verification result is not by itself authenticated-session proof and grants no capability; the separate step-up service binds TOTP to a rotated session, while each protected operation still needs its own authorization and proof check. Installation-admin actions not explicitly wired to that proof remain fail-closed.

## Implementation evidence

Slice 192 adds migration `0016_totp_credentials.sql`, `TotpCredentialStore`, and four content-free audit event kinds. Pending replacement, successful activation (including the consumed enrollment step), observed expiry deletion, replay-counter advancement, and each corresponding audit append share SQLite write transactions. Nine integration tests in `tests/mfa_credentials.rs` cover ciphertext-only persistence, the 10-minute boundary, wrong-code retry, pending replacement, inactive accounts, replay protection, transaction rollback, concurrent enroll/verify calls, audit-failure rollback, and schema constraints. This evidence validates internal code paths only; the exclusions above remain in force.
