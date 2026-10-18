# Sequoia/OpenPGP backup candidate — evaluation gates

**Status:** Evaluation prioritized by the operator on 2026-10-18; no production adoption.
**Owner decision:** Puxiri offline restore plus its recovery kit is sufficient. GnuPG/another external recovery tool is optional. Stable/mature APIs, security and single-binary constraints are unchanged.
**Canonical decision:** [ADR 0002](adr/0002-backup-archive-format-deferred.md).

## Current documentary evidence — 2026-10-18

- The [registry metadata](https://crates.io/api/v1/crates/sequoia-openpgp) and [versioned docs](https://docs.rs/crate/sequoia-openpgp/2.4.2) list **2.4.2**, published 2026-10-14, non-yanked, MSRV 1.85 and license LGPL-2.0-or-later. Existing project evidence used 2.4.1. Metadata does not prove builds on Puxiri's target matrix or approve the dependency.
- The [tagged Cargo manifest](https://gitlab.com/sequoia-pgp/sequoia/-/raw/openpgp/v2.4.2/openpgp/Cargo.toml) agrees on version/license. The [tagged NEWS](https://gitlab.com/sequoia-pgp/sequoia/-/raw/openpgp/v2.4.2/openpgp/NEWS) fetched in this review starts at changes in 2.4.1; it does not establish what changed in 2.4.2. A source/package comparison remains necessary before an isolated upgrade trial.
- The [official status page](https://sequoia-pgp.org/status/) still dates its implementation/audit statement to **2024-10-02**. It is historical evidence, not proof of the present audit status. No newer independent audit report was established by this bounded review.
- The [RustSec package page](https://rustsec.org/packages/sequoia-openpgp.html) lists the 2023, 2024 and 2025 advisories already recorded in the ADR. This is not a current lockfile scan, native-library CVE assessment or proof of absence of vulnerabilities.
- The [2.4.2 documentation](https://docs.rs/crate/sequoia-openpgp/2.4.2) still warns that RustCrypto does not provide the same guarantees as mature crypto libraries and documents explicit experimental/variable-time opt-ins. Do not use those flags as a route around the user's criteria. Its prose MSRV text says 1.67 while version metadata declares 1.85; use versioned manifest/registry evidence and verify the actual build rather than relying on that stale paragraph.
- The [openssl-src registry](https://crates.io/api/v1/crates/openssl-src) still lists `300.6.1+3.6.3` as the newest non-yanked 3.x source package. The official [2026-09-29 advisory](https://openssl-library.org/news/secadv/20260929.txt) directs 3.6 users to **3.6.5**. Earlier dynamic 3.6.4 evidence is no longer a current patch-floor demonstration. No exploit reachability through Sequoia was assessed here; do not claim either reachability or irrelevance. Merely selecting the newer 4.x source crate is not an approved backend/compatibility migration.

## Acceptance work, in order

1. Reconcile the exact 2.4.2 source/package change and inspect its resolved dependency/backend graph in an isolated scratch, without modifying product Cargo files. Refresh advisory evidence for that exact graph; separately account for native code.
2. Establish the current security-review evidence and a viable patched backend/distribution path. Preserve the single executable/no external archiver constraint. Verify embedded native versions rather than equating `cargo audit` with native patch coverage. Resolve licensing/distribution obligations before adoption; the crate's version alone does not settle them.
3. Specify a narrow modern authenticated-encryption/recovery-key profile, exact KDF preflight allowlist and global concurrency/resource bounds. Prior Argon2/salt tests are feasibility evidence, not selected parameters or a complete recovery UX.
4. Rerun version-specific stream/key round-trip and hostile-input tests, then evaluate bounded-memory full SQLite plus pinned-object bundle composition and isolated restore. Plaintext must remain private until authenticated EOF and inventory validation complete. Do not automatically port an experimental custom container into product code.
5. Verify durability/publication, crash/disk-full cleanup, key custody/import/export, offline restorer packaging/version compatibility and supported architectures. A successful synthetic stream test does not close these gates or setup/recovery requirements.

No code, dependency, crypto profile, schema, live provider or user data changed during this documentary review. No new runtime, build, cryptographic, performance or interoperability result is claimed. Candidate/backend/format approval remains a separate decision after the relevant evidence exists.
