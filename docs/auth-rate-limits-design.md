# Authentication rate-limit state — feature design

**Status:** Persistence and policy primitive implemented; no login route uses it yet.
**Requirements:** `docs/development-plan.md`, especially sections 8 and 19.2.

## Scopes and caller contract

Failed local-password attempts contribute to three independent buckets: client IP, canonical identity, and installation. Each scope has its own caller-supplied window, threshold, base backoff, and maximum backoff. The plan intentionally leaves production values to benchmark and installation-admin configuration; the primitive invents no defaults.

The caller checks `blocked_until` before an Argon2 operation, then records a failure or a successful identity after verification. Identity input must use the product's canonical email key (outer trim and ASCII case folding; dots and `+tag` remain significant). Client IP must come from the socket peer unless a specifically trusted proxy is configured. The primitive does not parse proxy headers or normalize an identity itself.

Backoff doubles after each threshold-reaching failure and caps at the configured maximum. Windows reset after their own duration; expired rows are removed by an explicit bounded maintenance call. Thus no counter creates a permanent lockout. A successful login clears only that identity's bucket; it does not erase IP-wide or installation-wide spray evidence. A currently active block is returned without adding more failure counts.

## Persistence and key lifecycle

Migration 0008 persists only HMAC-SHA-256 pseudonyms, counts, and UTC timestamps. IP/identity strings are not stored. HMAC domains distinguish IP, identity, and installation values. The limiter stores the non-secret master-key ID used to create pseudonyms. Opening with a different master key fails closed with `MasterKeyChanged`; do not silently reset counters or update the ID. A future master-key rotation procedure must explicitly migrate or retire this security state.

SQLite `BEGIN IMMEDIATE` serializes multi-scope failure updates, so two concurrent failing requests cannot overwrite one another. Existing global password-KDF admission still bounds active hashes in-process. This module itself does not reserve a KDF slot or provide distributed concurrency control.

## Not yet integrated

- No HTTP handler invokes these methods, maps a block to a response/`Retry-After`, or verifies the socket/proxy trust boundary.
- No installation settings UI or durable configuration loader supplies the three scope policies.
- No scheduler invokes bounded expiry cleanup.
- No request/body/IP cardinality admission cap or cross-process deployment contract exists; production wiring must prevent unbounded attacker-controlled bucket creation and benchmark it.
- This is not a substitute for generic login responses, CSRF/Origin checks, KDF admission, or account-state/session checks.
