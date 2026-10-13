# MFA TOTP enrollment and credential storage

- **Status:** Internal persistence/enrollment implemented in slice 192; transport and privileged assurance remain open
- **Date:** 2026-10-13
- **Related:** [ADR 0003 — TOTP secret encryption profile](adr/0003-totp-secret-encryption.md), `docs/development-plan.md` sections 7.1–7.2

## Boundary and actor

The actor is an active user configuring an authenticator for their own account. This slice supplies internal SQLite services only. A future HTTP caller must establish the authenticated session, current account state, CSRF/Origin policy, rate limits, and authorization before calling them. An arbitrary user UUID is not proof of identity. No setup wizard, browser flow, route, QR/otpauth URL, or session MFA assurance is added.

## Credential lifecycle

`mfa_credentials` holds at most one row per user and has two valid states:

| State | Stored representation | Allowed transition |
|---|---|---|
| No credential | No row | Active user begins enrollment → pending |
| Pending | 65-byte ADR 0003 encrypted envelope; `activated_at` and replay counter absent; expiry present | Correct TOTP before expiry → active; expiry → delete; begin again → atomically replace pending secret/id |
| Active | Encrypted envelope; activation time and last accepted TOTP step present; pending expiry absent | Verify a newer TOTP step → advance replay counter; re-enrollment/disable is rejected or absent until a separately authorized lifecycle exists |

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

There is no disable/revoke or replace-active operation, recovery-code flow, admin-MFA enforcement, password-only privileged path, session-bound proof, recent-assurance window, HTTP/MCP route, frontend, rate-limit wiring, audit viewer, automatic expiry sweeper, or master-key rotation. The transaction-level verification result is not by itself an authenticated-session proof and grants no capability. Installation-admin privileged operations stay fail-closed until a separate slice binds verified TOTP to the exact current session and protected mutation.

## Implementation evidence

Slice 192 adds migration `0016_totp_credentials.sql`, `TotpCredentialStore`, and four content-free audit event kinds. Pending replacement, successful activation (including the consumed enrollment step), observed expiry deletion, replay-counter advancement, and each corresponding audit append share SQLite write transactions. Nine integration tests in `tests/mfa_credentials.rs` cover ciphertext-only persistence, the 10-minute boundary, wrong-code retry, pending replacement, inactive accounts, replay protection, transaction rollback, concurrent enroll/verify calls, audit-failure rollback, and schema constraints. This evidence validates internal code paths only; the exclusions above remain in force.
