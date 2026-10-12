# Password reset — feature design

**Status:** One-time token issuance and password/session transaction implemented; email delivery and HTTP routes are not wired.
**Requirements:** `docs/development-plan.md`, sections 7.2 and 7.4.

`PasswordResetStore` issues a 256-bit opaque token only for an active account with an existing local password and a confirmed email. Email lookup uses the established trim + ASCII-case-fold comparison key. Only SHA-256(token), user/email references, creation, and expiry are persisted. Reissuing for the same address invalidates the prior unused token. The lifetime is required from the caller because the plan requires expiration but does not select a duration.

Reset prechecks token state/timestamps before doing Argon2, hashes the new password using the shared bounded `PasswordService`, then rechecks all eligibility under `BEGIN IMMEDIATE`. It consumes the token, updates the password, revokes all existing sessions, and invalidates other outstanding reset tokens in one transaction. A concurrent reset using the same token has one winner; failed downstream writes roll back token consumption, password replacement, and session revocation together. Malformed, future, expired, consumed, invalidated, or unknown tokens return the same absent outcome. Password-policy/KDF/database failures remain internal errors and must not be serialized directly to public callers.

The future request path must respond generically for known and unknown addresses, rate-limit issuance and consumption independently of the login buckets where appropriate, avoid putting bearer tokens in logs/referrers, deliver only through the configured email adapter, validate Origin/CSRF, and avoid open redirects. No email provider is contacted by this domain primitive. Password reset does not substitute for MFA recovery and does not let a password-disabled account gain local-password access.
