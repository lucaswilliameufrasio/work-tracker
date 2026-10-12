# Password authentication — feature design

**Status:** Domain orchestration implemented; no public HTTP login route or browser cookie is enabled.
**Requirements:** `docs/development-plan.md`, sections 7.2, 7.4, and 7.5.

## Domain flow

1. Canonicalize the submitted email using the established comparison key; malformed input shares one non-address identity bucket.
2. Check IP, identity, and installation rate-limit buckets before Argon2 work.
3. Verify only a confirmed email on an active account. Unknown, pending, inactive, credential-less, malformed, and wrong-password paths perform the dummy KDF and produce the same domain rejection.
4. Re-check rate-limit state after the KDF to observe a concurrent block where possible.
5. Issue a session only inside a SQLite `BEGIN IMMEDIATE` transaction that confirms the account remains active, the email remains confirmed, and the exact PHC string that was verified is still the stored credential. This prevents a password reset racing the KDF from issuing a new session after revocation.
6. Clear the identity failure bucket after a verified credential; keep IP and installation evidence.

The service returns only authenticated or rejected outcomes, with an optional internal block deadline for a later HTTP adapter. It does not expose why a credential failed. The PHC string stays in process memory for the verification-to-session compare and must never be logged.

## HTTP boundary deliberately deferred

Do not expose this service until the route has a canonical-origin policy, strict request-size limits, CSRF and Origin validation for cookie mutations, a shared generic error response, cookie `Secure`/`HttpOnly`/`SameSite=Lax`, session rotation, and the configured rate-limit policies. Proxy-derived IPs are not trusted unless the exact proxy is explicitly configured. Return a stable generic authentication failure; rate limiting may add a safe `Retry-After` value without identifying which bucket matched.

The service is not wired into `main` and does not decide CAPTCHA, self-registration, account approval, or MFA step-up. No setup wizard or initial owner is available yet.
