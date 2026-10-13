# TOTP multi-factor authentication — feature design

**Status:** TOTP, encrypted credential persistence, pending enrollment, session-bound step-up, internal disable of optional TOTP, restricted recovery start, and pending replacement enrollment are implemented; confirming TOTP replacement, recovery-code rotation during recovery, transport routes, MFA policy enforcement, and user-facing setup remain open.
**Requirements:** `docs/development-plan.md`, sections 7.1–7.2.

## Interoperable TOTP profile

`TotpSecret` generates 160 random bits, holds them in zeroizing memory, and exposes canonical unpadded RFC 4648 Base32 only through an explicitly named enrollment method. TOTP follows RFC 6238/RFC 4226 with HMAC-SHA-1 dynamic truncation. `TotpPolicy::authenticator_default` uses a 30-second step, six decimal digits, and at most one time step of tolerance backward and forward; callers may select six, seven, or eight digits and a smaller clock window. This keeps setup compatible with common authenticator applications while bounding online guesses.

Validation checks every candidate step with constant-time code comparison, rejects malformed/negative-time input, and returns the matched step only if it is newer than the caller's last accepted counter. The caller must persist that counter atomically with the protected operation; this primitive alone does not stop replay. RFC 6238 test vectors include timestamps beyond 2038.

## Secret lifecycle boundary

The secret must be shown only during the authenticated enrollment flow. Do not log, retain in browser storage, or persist the Base32 value. TOTP keys are persisted only in the ADR 0003 encrypted envelope and decrypted for verification. Internal persistence/enrollment is described in `docs/mfa-enrollment-design.md`; internal session rotation and recent assurance are described in `docs/mfa-session-step-up-design.md`. Neither design adds an HTTP/MCP enrollment route, QR/otpauth flow, or user-facing setup. Installation-admin TOTP is a product requirement but is not yet enforced through a product transport or bootstrap flow.

## One-time recovery codes

`RecoveryCodeStore` issues a caller-selected batch (bounded to 1–20) of 128-bit random Base64URL codes, persists only SHA-256 digests, and rotates outstanding codes transactionally. Codes are user-scoped, only active accounts may issue or consume them, they have no implicit TTL, and successful consumption is exactly once. `consume_in_transaction` lets the future recovery flow commit the code and recovery-session/action together; callers must use `BEGIN IMMEDIATE`, roll back on any downstream failure, and apply authentication rate limits before accepting attempts. Reissue failure rolls back invalidation of the prior batch. The plaintext is returned only in a zeroizing batch and can never be retrieved from storage.

Recovery start is internal only and described in `docs/mfa-recovery-design.md`: a non-installation-admin needs a current password and one recovery code; success issues a separate 10-minute recovery-only token, never a normal session or MFA proof. That token can start one encrypted replacement enrollment, whose deadline is capped by the recovery token; the old TOTP remains active and a repeated start will not replace or redisplay the pending seed. Confirming/replacing the credential, recovery-time code rotation, backup/restore of encrypted credentials, generic login MFA challenge, transport, and policy enforcement remain follow-up work. Optional TOTP can be disabled only with a current MFA proof; installation admins cannot disable their mandatory factor. The portable recovery kit is still blocked by ADR 0002's deferred format/security decision. Internal session-bound TOTP step-up exists, but does not consume recovery codes or itself authorize privileged actions.

## Sources

- [RFC 6238, TOTP](https://www.rfc-editor.org/rfc/rfc6238) — 30-second default, HMAC construction, synchronization window, replay rule, and test vectors.
- [RFC 4226, HOTP](https://www.rfc-editor.org/rfc/rfc4226) — HMAC-SHA-1 dynamic truncation and decimal code generation.
