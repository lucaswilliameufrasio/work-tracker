# ADR 0007: Local disaster recovery and installation administration

- **Status:** Accepted
- **Superseded in part on 2026-11-09 by [ADR 0008](0008-replication-to-s3-compatible-storage.md):** each capture can also be uploaded to an S3-compatible destination and restored from it. The statement that Puxiri never calls the library's S3 client no longer holds.
- **Date:** 2026-11-09
- **Context:** [ADR 0006](0006-optional-muira-login-and-usable-v1.md), [ADR 0002](0002-backup-archive-format-deferred.md), `docs/development-plan.md` section 13

## Context

ADR 0006 made the first usable installation depend on setup, the first project, comments, a personal item view, invitation email through Sabiá, MillionSend, and Resend, and MCP. It named `sqlite-replication` as the disaster-recovery library and left the administrative panel until after that cut. ADR 0002 still has no chosen encrypted archive format.

The operator will not have that encrypted backup. The installation still needs a recovery path, and it needs the screen that manages the business: people, projects, email, invitations, audit, and recovery. Those are not a later extra.

`sqlite-replication` captures one coordinated SQLite writer on Linux x86-64. Its local spool does not claim power-loss safety. Its object-store path is not a homologated production store. A replica stored on the same disk as `puxiri.sqlite3` does not survive loss of that disk.

## Decision

- Disaster recovery of the Puxiri SQLite database uses the Rust library in `sqlite-replication`. Puxiri takes a snapshot when the repository has none, then calls `capture_once` only after its own write gate has stopped new immediate writers and waited for the ones already inside a transaction. Restore writes a new server-named file under the data directory's `restores` folder. It never replaces the live database while Puxiri is running. Substituting that file is a manual step done with the process stopped.
- The replication repository defaults to `{data_dir}/replication`. `puxiri serve --replication-dir` / `PUXIRI_REPLICATION_DIR` can point it at another filesystem. The administration screen says when the replica shares the database disk, and that this does not survive loss of that disk.
- Puxiri does not upload the repository, does not call the library's S3 client, and does not promise recovery from power loss. ADR 0002 stays Deferred.
- The administration panel is `/gestao`. Its API is under `/api/v1/admin/`. Only an installation administrator can use it. An organization owner who is not an installation administrator cannot configure mail, ban people, or run recovery. The panel shows login source, mail configuration, replication status, people and ban/unban, project metadata, the three email senders, a link to invitations, and recent audit rows. It does not show issue text and it does not edit tables.
- `database-migrator` is still later. Applied SQL migrations stay as they are.

## Alternatives considered

- Waiting for the encrypted bundle in ADR 0002 leaves the installation without a recovery path the operator can run.
- Treating a same-disk copy as disaster recovery would fail when the disk itself is the lost component.
- Restoring over the live `puxiri.sqlite3` from the running process can truncate the database the process still has open.
- Putting the screen at `/admin` collides with the SPA fallback, which reserves that prefix for JSON 404s.
- Letting an organization owner administer the installation would blur two authorities the access model already separates. Mail configuration already requires the installation administrator.

## Consequences and limits

- Recovery is proven only for a local repository on Linux x86-64, into a new file, for the rows captured before the restore. It is not a production object-store replica and not a power-loss guarantee.
- Puxiri pins rusqlite 0.39.0 so this binary and sqlx 0.9.0 share one `libsqlite3-sys` 0.37.0 (bundled SQLite 3.51.3). That is the newest rusqlite sqlx 0.9 can link, and it still meets sqlite-replication's minimum of SQLite 3.51.3. The replication library's own workspace may resolve rusqlite 0.41.1 when built alone. The backup spike and capture inside Puxiri share the pinned SQLite.
- Writes that do not take the immediate write gate can still race a capture. Product immediate transactions go through the gate.
- The administration panel does not include S3 settings, CAPTCHA, Prometheus, or usage charts from development-plan section 13.
- ADR 0006 remains the record for Muira, the three email providers, and MCP. This record only changes when recovery and the administration panel arrive.

## Evidence

- On 2026-11-09 the operator said that, without the backup, disaster recovery still has to be the `sqlite-replication` project, and the administrative panel also has to exist because it is management of the whole business.
- The local `sqlite-replication` library documents `snapshot`, gated `capture_once`, and restore onto a path that must not already exist. Its own ADR limits that capture to Linux x86-64, one writer, and no power-loss or production object-store claim.
