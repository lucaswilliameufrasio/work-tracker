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

## Package reconciliation and isolated native-build evaluation — 2026-10-19

Downloaded both published `.crate` archives and checked each SHA-256 against its versioned crates.io metadata before comparing regular-file contents (no added/removed paths; 18 changed files among 651 files in each archive). Archive inspection rejected absolute/traversal paths, duplicate members and non-regular/non-directory entries and bounded download/unpacked sizes. This checks artifact identity against the registry, not an independent source audit or reproducible build.

| Package | SHA-256 | Embedded VCS revision |
| --- | --- | --- |
| Sequoia 2.4.1 | `0fbc8f9818a6fad141d85993777ba298ee52c80a09bd23d45dda461a6b7c83cb` | `0b0c8c7f038b829de2da0d28a822941d8600f3ee` |
| Sequoia 2.4.2 | `1460dbd9c736920a78bd450eaa6902304605c493bd2222121849ba01ab8df997` | `eba129ea3d097a524acccea9ba14eb8e6751a001` |

The packaged `NEWS` is unchanged. The actual diff includes a minimum 1,024-byte stream-verifier buffer and an underflow regression test, OpenSSL RIPEMD160 mapping, RustCrypto RNG changes, `rand` 0.10.1 and `ossl` 1.5.3 minimum dependency updates, certificate/regex/timing-macro changes and SLH-DSA-SHAKE documentation clarifications. These are source observations, not proof that all behavioral/security implications have been reviewed. In particular, newly supported algorithms must not automatically enter a future backup allowlist. The 2.4.1 tests are still not evidence for 2.4.2.

The [official OpenSSL download table](https://openssl-library.org/source/) lists 3.6.5 with end-of-life **2026-11-01**, and **3.5.9 LTS** with support through **2030-04-08**. For isolated evaluation only, downloaded the official 3.5.9 source archive and verified SHA-256 `603f5602e2eef00d77fbd429d34dcd5822bb301757a1bc9cdb24c670f1eb859a` plus its detached signature against the official primary-certificate fingerprint `B146647E45A7B33947AB226B2A2C87D161692D40` in a fresh scratch GnuPG home. The trust anchor was the fingerprint published over official HTTPS, not an independently established trust chain. GnuPG here verifies a third-party source artifact; it does not become a Puxiri recovery requirement. No host OpenSSL, system keyring or product dependency was changed.

### One-host static native-link smoke result

Built that verified OpenSSL source in an ignored private checkout directory with `./Configure --prefix=<scratch>/install --libdir=lib no-shared no-tests no-apps no-docs`, `make -j2` and `make install_sw`. OpenSSL's own test suite was explicitly **not** built/run. `no-shared` still produced a separate legacy provider module; the successful smoke below did not mount that module, but this does not prove every future algorithm/profile works without modules.

Created a separate unpublished Rust scratch package containing the **unmodified** upstream `examples/generate-encrypt-decrypt.rs` from Sequoia 2.4.2, with only this direct dependency:

```toml
sequoia-openpgp = { version = "=2.4.2", default-features = false, features = ["crypto-openssl"] }
```

With Rust/Cargo **1.99.0 via mise**, resolved a scratch lockfile, recorded `cargo tree --locked -e features`, and ran `OPENSSL_DIR=<scratch>/install OPENSSL_STATIC=1 CARGO_BUILD_JOBS=2 mise exec -- cargo run --release --locked`. Build and the example's key generation/encrypt/decrypt/plaintext equality assertion passed. ELF `NEEDED` lists only `libgcc_s.so.1`, `libc.so.6` and `ld-linux-x86-64.so.2`, not OpenSSL shared libraries; executable strings include `OpenSSL 3.5.9 29 Sep 2026`. This is **static OpenSSL linkage**, not a fully static executable.

The same binary passed again in an existing `rust:1.95-trixie` rootless Docker image with `--pull=never --network none --read-only --cap-drop ALL --security-opt no-new-privileges --cpus 1 --memory 1g --pids-limit 64 --user 65534:65534`, mounting **only the binary** read-only at `/smoke` and overriding the entrypoint. No source/install directory, scratch OpenSSL module or secret was mounted. This is a small synthetic execution under configured limits, not a cgroup memory measurement, minimal-image deployment certification, portable/reproducible build or supported architecture matrix.

`cargo audit --json` **0.22.1** passed for the scratch lockfile: 154 dependencies including the root, zero reported vulnerabilities and no warnings against advisory DB revision `999ec89d5d3f10f20be14750ec8c2d8b44e0ac9b` (last updated 2026-10-17). Scratch lockfile SHA-256: `6aee8e1d1c3929bb3bc21665c6d75c6bca75851667351aaa1a9e418ec8d09822`. This does not scan the separately built OpenSSL source or guarantee absence of vulnerabilities. Binary SHA-256 for this local build: `c42eb5d403caa6b588baf6ee695b0daecd7014d16d52038dbcc91f9188bfbe18`; hashes identify artifacts, not reproducibility.

The example collects a tiny message in `Vec`s, uses upstream defaults and illustrative helpers, and has no hostile-input, AEAD/KDF allowlist, signature policy, full-bundle, recovery-key import/export, interruption or concurrency verification. It is not product code or a proposed production profile. The package-diff gate now has concrete evidence and a patched-LTS native-build path is locally feasible, but current independent security review, licensing/distribution obligations, version-specific robust stream tests and all full recovery acceptance gates remain open. No product quality-gate rerun is claimed for this documentation-only checkout change; no product dependency/code/schema changed.
