# TOTP credential lifecycle — feature design

**Status:** Internal disable of optional TOTP implemented in slice 194; replacement, recovery access, and product transport remain open.
**Requirements:** `docs/development-plan.md`, sections 7.1–7.2.
**Related:** [TOTP enrollment](mfa-enrollment-design.md), [session step-up](mfa-session-step-up-design.md), [TOTP profile](mfa-design.md).

## Actor and operation

An active user who still controls their enrolled authenticator may disable optional TOTP for their own account. The service accepts only a fresh `RecentMfaProof`; it does not accept a user ID or password-only proof. The proof is revalidated against the exact unrevoked session, token digest, account, and persisted MFA marker in the same write transaction.

Installation-admin TOTP is mandatory in the product plan, so this self-service operation always rejects installation admins. A lost-device/admin recovery path must be implemented separately using the planned one-time recovery codes; it must not be emulated by password-only disable. Organization/admin/project-manager MFA requirements are configurable in the product plan but their settings are not implemented yet; a future policy layer must be checked before exposing disable through transport.

On successful disable, one `BEGIN IMMEDIATE` transaction deletes the active encrypted credential, clears `mfa_authenticated_at` on all of that user's sessions (invalidating any outstanding in-memory `RecentMfaProof`), appends a content-free audit event, and commits. Existing sessions stay signed in as ordinary sessions; the operation neither elevates nor revokes them. If any persistence/audit action fails, all effects roll back. Removing ciphertext from SQLite is not a claim of forensic erasure from old database pages or backups.

No active-credential replacement, pending-enrollment cancellation, recovery-code consumption/issuance, login challenge, HTTP/MCP route, UI, CSRF/Origin control, rate-limit wiring, or policy configuration is included. Users who lose an authenticator cannot use this operation; recovery remains fail-closed until a separately designed recovery session and credential-reset journey exists.
