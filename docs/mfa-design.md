# TOTP multi-factor authentication — feature design

**Status:** TOTP and hash-only recovery-code primitives are implemented; no enrollment, persistence of TOTP secrets, recovery access flow, or login step-up is wired.
**Requirements:** `docs/development-plan.md`, sections 7.1–7.2.

## Interoperable TOTP profile

`TotpSecret` generates 160 random bits, holds them in zeroizing memory, and exposes canonical unpadded RFC 4648 Base32 only through an explicitly named enrollment method. TOTP follows RFC 6238/RFC 4226 with HMAC-SHA-1 dynamic truncation. `TotpPolicy::authenticator_default` uses a 30-second step, six decimal digits, and at most one time step of tolerance backward and forward; callers may select six, seven, or eight digits and a smaller clock window. This keeps setup compatible with common authenticator applications while bounding online guesses.

Validation checks every candidate step with constant-time code comparison, rejects malformed/negative-time input, and returns the matched step only if it is newer than the caller's last accepted counter. The caller must persist that counter atomically with the protected operation; this primitive alone does not stop replay. RFC 6238 test vectors include timestamps beyond 2038.

## Secret lifecycle boundary

The secret must be shown only during the authenticated enrollment flow. Do not log, retain in browser storage, or persist the Base32 value. TOTP keys must be encrypted at rest and decrypted only for verification. The product's authenticated-encryption/AAD and key-rotation contract has not yet been selected, so no MFA secret table or activation route is added here. Installation-admin TOTP is not yet mandatory in the product because there is no enrollment/recovery flow.

## One-time recovery codes

`RecoveryCodeStore` issues a caller-selected batch (bounded to 1–20) of 128-bit random Base64URL codes, persists only SHA-256 digests, and rotates outstanding codes transactionally. Codes are user-scoped, only active accounts may issue or consume them, they have no implicit TTL, and successful consumption is exactly once. `consume_in_transaction` lets the future recovery flow commit the code and recovery-session/action together; callers must use `BEGIN IMMEDIATE`, roll back on any downstream failure, and apply authentication rate limits before accepting attempts. Reissue failure rolls back invalidation of the prior batch. The plaintext is returned only in a zeroizing batch and can never be retrieved from storage.

This is not a recovery kit or lost-device flow. No MFA state is associated with the codes yet, and no route turns one into a restricted recovery session. MFA reauthentication, backup/restore of encrypted credentials, enrollment cancellation, lost-device recovery, and generic login step-up remain follow-up work. The portable recovery kit is still blocked by ADR 0002's deferred format/security decision.

## Sources

- [RFC 6238, TOTP](https://www.rfc-editor.org/rfc/rfc6238) — 30-second default, HMAC construction, synchronization window, replay rule, and test vectors.
- [RFC 4226, HOTP](https://www.rfc-editor.org/rfc/rfc4226) — HMAC-SHA-1 dynamic truncation and decimal code generation.
