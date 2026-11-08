# Password authentication — feature design

**Status:** Password authentication and the internal MFA login challenge are implemented; no public HTTP login route or browser cookie is enabled.
**Requirements:** `docs/development-plan.md`, sections 7.2, 7.4, and 7.5.

## Domain flow

1. Canonicalize the submitted email using the established comparison key; malformed input shares one non-address identity bucket.
2. Check IP, identity, and installation rate-limit buckets before Argon2 work.
3. Verify only a confirmed email on an active account. Unknown, pending, inactive, credential-less, malformed, and wrong-password paths perform the dummy KDF and produce the same domain rejection.
4. Re-check rate-limit state after the KDF to observe a concurrent block where possible.
5. Inside a SQLite `BEGIN IMMEDIATE` transaction, recheck the exact verified PHC, confirmed email, active account, and current installation-admin/TOTP state. A normal session is issued only when no TOTP factor is required. Active TOTP users receive a distinct short-lived MFA challenge and no session; an installation admin without an active factor receives `MfaSetupRequired` and no challenge/session.
6. The challenge stores only a domain-separated digest of its opaque 256-bit bearer and a fingerprint of the verified PHC, together with the user, confirmed email key, active TOTP credential ID, and ten-minute expiry. Completion revalidates every binding, checks the existing IP/identity/installation limiter inside the write transaction, consumes a strictly newer TOTP step, consumes the challenge, issues a session with persisted MFA assurance, clears only the identity failure bucket, and commits atomically.
7. Wrong TOTP records a shared rate-limit failure in the same transaction without consuming the challenge; expired, replayed, stale-password, removed-factor, and unknown challenges fail closed. A correct password alone does not clear the identity failure bucket when TOTP remains outstanding.

The password phase can return an authenticated session, an MFA challenge, an MFA-setup-required outcome, or a generic rejection with an optional internal block deadline. MFA completion returns only authenticated or rejected outcomes. The PHC string stays in process memory for password-stage checks; only a domain-separated SHA-256 fingerprint is persisted to bind a pending challenge to that exact password credential. Never log the PHC, challenge token, TOTP seed, or code.

## HTTP boundary deliberately deferred

Do not expose either phase until the routes have a canonical-origin policy, strict request-size limits, CSRF and Origin validation for cookie mutations, a shared generic error response, cookie `Secure`/`HttpOnly`/`SameSite=Lax`, the correct session-cookie boundary (no session cookie before MFA completion), and configured rate-limit policies. Proxy-derived IPs are not trusted unless the exact proxy is explicitly configured. Return a stable generic authentication failure; rate limiting may add a safe `Retry-After` value without identifying which bucket matched.

The service is not wired into `main` and does not decide CAPTCHA, self-registration, account approval, external-provider login, or configurable MFA requirements for organization/project roles. An installation admin without TOTP fails closed with `MfaSetupRequired`; bootstrap/enrollment setup must provision the factor before any normal session. No setup wizard or initial owner is available yet.
