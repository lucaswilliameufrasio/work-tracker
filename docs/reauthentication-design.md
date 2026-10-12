# Recent password reauthentication — feature design

**Status:** Password reauthentication and session-bound proof implemented; no HTTP endpoint, rate-limit adapter, MFA/provider proof, or privileged-action UI is wired.
**Requirements:** `docs/development-plan.md`, sections 7.2, 7.4, and 8.

`ReauthenticationService` first validates the opaque session, then verifies the active user's current local password using the shared bounded Argon2 service. On success it opens `BEGIN IMMEDIATE` and `SessionStore::record_reauthentication` checks the same session ID, user ID, bearer digest, active account, unrevoked/within-lifetime session, and exact PHC hash that was verified. The marker write is therefore rejected if a password reset/change wins between KDF verification and persistence.

The session already has a nullable `reauthenticated_at` column; no schema change was needed. The resulting `RecentReauthentication` is an opaque, non-serializable value bound to the user, session, bearer digest, timestamp, and conservative session-valid-until bound. Email address mutations require this proof, recheck it against persisted current session state in their own transaction, and enforce the plan's ten-minute privileged window. The account's current installation-admin status is queried at mutation time; password-only proofs are refused for installation admins because those sensitive changes require MFA.

The HTTP boundary must apply request-size limits, socket/trusted-proxy IP policy, per-IP/identity/installation rate limits before the KDF, Origin and CSRF validation, and generic error translation. Do not expose whether the password was wrong, an account lacked a local credential, the session was invalid, or a request hit a rate bucket. TOTP step-up and linked-provider reauthentication are future proof factors; no bypass is available for installation admins until they exist.
