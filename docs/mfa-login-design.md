# MFA login challenge — feature design

**Status:** Internal domain design for the next login slice; no HTTP route, browser session cookie, or user interface is included.
**Requirements:** `docs/development-plan.md`, sections 7.1–7.2.
**Related:** [Password authentication](password-login-design.md), [TOTP profile](mfa-design.md), [TOTP session step-up](mfa-session-step-up-design.md), [TOTP enrollment](mfa-enrollment-design.md).

## Actors and journey

An active user with a confirmed email and password can sign in. A user with an active TOTP credential must prove both factors; an installation admin must have an active TOTP credential before receiving any normal session. Accounts without active TOTP and without a mandatory MFA role retain the existing password-only flow.

1. Apply the existing IP, identity, and installation rate limits before password KDF work, then verify the confirmed email and password.
2. In one `BEGIN IMMEDIATE` transaction, revalidate the exact password PHC, confirmed email, active account, and current installation-admin/TOTP state.
3. For an active TOTP credential, persist only a digest of a separate 256-bit, opaque login-challenge token. Return a ten-minute challenge; do not create a session. For an installation admin without an active factor, return `MfaSetupRequired` and do not create a challenge or session. Otherwise issue a normal session.
4. The challenge endpoint resolves the identity from its persisted record, checks the existing rate-limit buckets before decrypting/verifying TOTP, and in one `BEGIN IMMEDIATE` transaction revalidates the exact challenge, account, confirmed email, password-credential fingerprint, and bound active TOTP credential.
5. On a newer valid TOTP step, consume the challenge, advance the anti-replay counter, issue a normal session with persisted MFA assurance, clear only the successful identity rate-limit bucket, and commit atomically. An invalid code records a rate-limit failure without consuming the challenge; expiry, replay, changed credentials, revoked factor, or used challenge fail closed. Audit failure rolls back the challenge, session, replay counter, and rate-limit success together.

## State and security boundaries

Password verification transitions to one of: an authenticated session for accounts not requiring TOTP; a restricted `MfaRequired` challenge with no normal session; or `MfaSetupRequired` with no session for a mandatory installation admin missing a factor. A challenge transitions only to authenticated-and-consumed, expired, or rejected; it is not a session token, recovery token, or MFA proof.

The challenge record binds the token digest to the user, confirmed email key, password credential fingerprint, and exact active TOTP credential ID. A password change or credential replacement therefore invalidates an in-flight challenge. Deleting the bound TOTP credential cascades the challenge. Challenge and session creation, and challenge consumption/session issuance, are transactionally coupled. Password or TOTP failures share the existing persisted IP/identity/installation backoff; a correct password alone does not clear the identity failure bucket when another factor remains outstanding.

All TOTP-enabled accounts require their active factor at login, including accounts that enabled it voluntarily. The only mandatory role implemented here is installation admin. MFA requirements for organization owners/admins and project managers remain disabled until their product policy is approved and configured. An installation admin without TOTP cannot fall back to password-only; authenticated setup, bootstrap provisioning, and recovery remain separate journeys.

## Module ownership and alternatives

- `PasswordAuthenticator` owns the password-first public domain outcome and delegates the post-password state transition.
- `MfaLoginService` owns challenge lifecycle, credential/factor revalidation, rate-limited TOTP completion, and transactional coordination.
- `TotpCredentialStore` owns TOTP decryption, verification, audit, and monotonic step consumption.
- `SessionStore` owns session insertion and persisted MFA assurance.
- `AuthRateLimiter` owns shared authentication buckets and must support checks/failure writes inside the challenge transaction.

A normal session with a “limited” flag was rejected: existing session validation and authorization boundaries would make that easy to mistake for an authenticated session. Reusing an MFA recovery token was rejected because recovery has different authority and a separate lifecycle. A stateless signed challenge was not selected because the application has no established signing-key lifecycle for these login assertions; the database record supports one-time consumption and credential revalidation in the same transaction as session issuance.

## Deferred work

HTTP/MCP transport, secure cookies, CSRF/Origin checks, request-size limits, setup enrollment UX, login MFA challenges for external providers, configurable role policy, audit viewing, expiry maintenance, and live-provider validation remain separate work. Do not expose this service through a public route until the HTTP boundary requirements in `password-login-design.md` are met.
