# ADR 0003: TOTP secret encryption profile

- **Status:** Accepted for the internal TOTP envelope; enrollment and persistence remain open
- **Date:** 2026-10-12
- **Context:** `docs/development-plan.md`, sections 7.1–7.2 and 8; ADR 0002 remains separate and deferred

## Context

RFC 6238 verification needs the TOTP seed, so unlike one-time recovery codes it cannot be stored only as a digest. Persisting a seed in plaintext would expose every authenticator secret to a database-only disclosure. The application already has a 32-byte local master key outside SQLite. The user selected `aws-lc-rs` for this boundary; that choice does not select the deferred backup/recovery archive format.

## Decision

- Use the exact `aws-lc-rs` 1.18.2 crate with its `aws-lc-sys` feature and without unrelated default features.
- Use AES-256-GCM with the library's `RandomizedNonceKey`, which generates a 96-bit nonce for each seal; use the standard 128-bit authentication tag. Do not implement nonce generation or AES/GCM primitives in application code.
- Derive a separate 256-bit key for each `(user UUID, credential UUID)` from the master key using HKDF-SHA-256 and fixed, versioned domain-separation context. Keep derived key bytes zeroized and never expose master-key bytes to MFA callers.
- Encode one strict 65-byte binary envelope: version byte, 16-byte master-key identifier, 12-byte nonce, and 36-byte ciphertext-plus-tag for the 20-byte RFC 6238 secret. Authenticate the version, key identifier, user UUID, credential UUID, and a fixed purpose/domain string as AAD.
- Reject unknown envelope versions, wrong master-key identifiers, malformed lengths, and authentication failures. Do not silently fall back to plaintext, another key, or another algorithm. A format/profile change requires a new version and an explicit migration.
- This only establishes the cryptographic boundary. No database schema, enrollment/activation/revocation flow, admin MFA enforcement, session assurance, routes, or backup format is approved by this ADR.

## Alternatives considered

- RustCrypto XChaCha20-Poly1305 was the original recommended alternative, but the user selected AWS-LC instead.
- OpenSSL AES-GCM was not selected; direct use of `aws-lc-rs` keeps the chosen crypto API explicit and shares the AWS-LC implementation already present transitively in the locked dependency graph.
- Deferring encrypted TOTP storage was rejected by the user's explicit library choice, but privileged MFA flows remain blocked until persistence, enrollment, and enforcement are implemented.

## Consequences and limits

- The crate builds AWS-LC native code and requires a supported C/C++ compiler; the repository already builds AWS-LC transitively for its AWS SDK dependency. Revalidate supported targets in the normal build matrix; ARM64 cross-build remains unverified.
- Selecting `aws-lc-rs` does **not** mean this application is running a FIPS-validated configuration. No FIPS mode or compliance claim is made.
- Key rotation is not implemented. A changed master key is detected by the key identifier and causes fail-closed decryption; a future rotation must preserve old-key selection and transactional re-encryption.
- Encryption at rest does not protect against a compromised running process, host, or master key. Database backups containing envelopes still require the separately deferred backup encryption and recovery design.
- Secret persistence is safe only through this versioned envelope; do not add plaintext fallback or make password-only authentication a path to installation-admin authority.

## Evidence and references

- User's explicit selection of `aws-lc-rs` on 2026-10-12.
- [`aws-lc-rs` 1.18.2 AEAD API](https://docs.rs/aws-lc-rs/1.18.2/aws_lc_rs/aead/index.html) and [`RandomizedNonceKey`](https://docs.rs/aws-lc-rs/1.18.2/aws_lc_rs/aead/struct.RandomizedNonceKey.html).
- [`aws-lc-rs` 1.18.2 HKDF API](https://docs.rs/aws-lc-rs/1.18.2/aws_lc_rs/hkdf/index.html); crate metadata reports Rust 1.71 MSRV.
- [AWS-LC Rust build requirements FAQ](https://aws.github.io/aws-lc-rs/faq.html); non-FIPS builds require a C/C++ compiler, while the additional FIPS toolchain/configuration is not enabled here.
