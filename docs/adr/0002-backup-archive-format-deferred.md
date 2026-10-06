# ADR 0002: Backup archive format selection is deferred

- **Status:** Deferred — security and product compatibility evidence required
- **Date:** 2026-10-06
- **Context:** `docs/development-plan.md`, backup sections 12.2–12.3 and Phase 0 evidence 6–8, 45–46

## Context

Puxiri backups must remain self-hosted and usable in the single Rust binary. A full bundle must stream the SQLite snapshot and pinned remote files, compress/encrypt with bounded memory, restore into an isolated staging area, and support an offline recovery kit. Production must not require a Node runtime or an external archiver process. Paths, archive entry types/sizes, checksums, publication, interruption recovery and key custody are part of the format boundary.

The existing complete-bundle tests are a Node Phase 0 harness, not production code. A separate Rust scratch spike combined `tar` 0.4.44, `flate2` 1.1.9 with the pure-Rust backend, and `age` 0.12.1 streaming APIs. It encrypted/decrypted 128 MiB of synthetic data under a 1 CPU/1 GiB cgroup, verified the payload, and rejected a wrong identity and modified ciphertext. This proves basic composition and bounded streaming for that fixture only. The `age` crate explicitly remains beta before 1.0 even though its file format has a stable v1 specification; no real archive was tested against the `rage` CLI/reference implementation, nor with Puxiri snapshots, S3 pins, restoration policy or recovery keys.

## Decision

Do not select a production archive format or add `age`, `tar`, or compression/encryption dependencies to the product yet. Keep the current Node bundle harness experimental and retain all backup/restore requirements. Do not implement a custom general-purpose encryption/container protocol as a shortcut around the candidate's maturity status.

Before resuming, evaluate whether Puxiri accepts the age Rust crate's pre-1.0 API lifecycle and can pin/audit a reviewed release, or identify a maintained Rust implementation with a stable authenticated streaming format. Whichever candidate is considered must prove format interoperability, recovery-key import/export, secure allowlisted TAR restoration, full SQLite snapshot plus pinned object round-trip, crash/disk-full behavior, atomic publication and cgroup memory/resource bounds.

## Consequences

- No production backup format, endpoint, migration or restore flow is approved by this ADR.
- The Rust age/TAR result is feasibility evidence, not production security approval or a capacity claim.
- Phase 0 backup/restore and resource checkboxes remain open until the end-to-end contracts above are verified and recorded.
- S3 remains optional; a backup's key custody and restored destination credentials remain separate from this format selection.

## Evidence

- `docs/development-plan.md`, Phase 0 and evidence 6–8, 45–46.
- Source links and the temporary probe's exact versions, limits and omissions are recorded in evidence 46.
