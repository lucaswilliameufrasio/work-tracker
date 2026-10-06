# ADR 0002: Backup archive format selection is deferred

- **Status:** Deferred — security and product compatibility evidence required
- **Date:** 2026-10-06
- **Context:** `docs/development-plan.md`, backup sections 12.2–12.3 and Phase 0 evidence 6–8, 45–46, 49, 51–53, 56–57, 60–61

## Context

Puxiri backups must remain self-hosted and usable in the single Rust binary. A full bundle must stream the SQLite snapshot and pinned remote files, compress/encrypt with bounded memory, restore into an isolated staging area, and support an offline recovery kit. Production must not require a Node runtime or an external archiver process. Paths, archive entry types/sizes, checksums, publication, interruption recovery and key custody are part of the format boundary.

The existing complete-bundle tests are a Node Phase 0 harness, not production code. A separate Rust scratch spike combined `tar` 0.4.44, `flate2` 1.1.9 with the pure-Rust backend, and `age` 0.12.1 streaming APIs. It encrypted/decrypted 128 MiB of synthetic data under a 1 CPU/1 GiB cgroup, verified the payload, and rejected a wrong identity and modified ciphertext. A follow-up used the Go `filippo.io/age` v1.3.2 reference CLI: Go decrypted and validated the Rust-produced 128 MiB TAR+Gzip stream, while Rust decrypted a short Go-produced ciphertext. This establishes stream-level v1 interoperability for these fixtures, not a Puxiri archive round-trip. The `age` crate explicitly remains beta before 1.0; neither implementation was tested with Puxiri snapshots, S3 pins, restoration policy or recovery keys.

## Decision

Do not select a production archive format or add `age`, `tar`, or compression/encryption dependencies to the product yet. Keep the current Node bundle harness experimental and retain all backup/restore requirements. Do not implement a custom general-purpose encryption/container protocol as a shortcut around the candidate's maturity status.

Before resuming, evaluate whether Puxiri accepts the age Rust crate's pre-1.0 API lifecycle and can pin/audit a reviewed release, or identify a maintained Rust implementation with a stable authenticated streaming format. Stream-level age v1 interoperability now has fixture evidence; the selected candidate must still pass recovery-key import/export, secure allowlisted TAR restoration, full SQLite snapshot plus pinned object round-trip, crash/disk-full behavior, atomic publication and cgroup resource bounds.

## Consequences

- No production backup format, endpoint, migration or restore flow is approved by this ADR.
- The Rust age/TAR result is feasibility evidence, not production security approval or a capacity claim.
- Age v1 stream interoperability with the Go reference implementation is verified only for the synthetic fixtures recorded in evidence 49; this does not approve the beta Rust crate or complete bundle format.
- Evidence 51 measures one synthetic Rust archive/snapshot/object composition and isolated restore on tmpfs and local Btrfs; it does not exercise the Puxiri pin/S3 protocol, production manifest or failure contracts, and is not a format selection or capacity claim.
- Evidence 52 rejects a wrong identity and a modified final ciphertext byte in that scratch archive; it does not approve the Rust crate or product publication/recovery behavior.
- Evidence 53 uploads/downloads the synthetic archive with an explicit checksum to disposable local MinIO and restores the downloaded bytes; it does not test presigned/browser flow, S3-provider compatibility, Puxiri pins, or a production manifest.
- Evidence 56 rejects a one-byte-truncated completed archive when the Rust age stream is drained; this is not a matrix of truncation points or product staging/crash behavior.
- Evidence 57 rejects one authenticated relative traversal path before opening an extracted file; it is a scratch allowlist probe, not a complete archive-path security review or product staging implementation.
- Evidence 60 rejects a duplicate path in an authenticated scratch archive. Evidence 61 adds process-level staging cleanup after the tested path-traversal and duplicate-path rejections, with a regression that first failed against the scratch implementation before the guard was added. These do not establish cleanup after process death, resistance to local path races, or product restore safety.
- Phase 0 backup/restore and resource checkboxes remain open until the end-to-end contracts above are verified and recorded.
- S3 remains optional; a backup's key custody and restored destination credentials remain separate from this format selection.

## Evidence

- `docs/development-plan.md`, Phase 0 and evidence 6–8, 45–46, 49, 51–53, 56–57, 60–61.
- Source links and the temporary probes' exact versions, limits and omissions are recorded in evidence 46, 49, 51–53, 56–57 and 60–61.
