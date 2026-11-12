# ADR 0008: Replication to S3-compatible storage

- **Status:** Accepted
- **Date:** 2026-11-09
- **Amended:** 2026-11-12 — scheduled capture and retention through generations
- **Context:** [ADR 0007](0007-disaster-recovery-and-administration.md), [ADR 0002](0002-backup-archive-format-deferred.md)

## Context

ADR 0007 kept disaster recovery local: Puxiri captured into a repository on this server and did not call the S3 client of `sqlite-replication`. A replica on the same machine does not survive loss of that machine.

The S3 work in `sqlite-replication` is now on its `main` branch. `sync_to_s3` uploads a validated local chain with immutable keys and writes a destination-scoped acknowledgement only after every object write is confirmed. `restore_from_s3` stages the remote chain in temporary space, verifies it, and restores to a path that must not exist. That library has tested RustFS 1.0.4 and Floci locally. It has not validated AWS S3, R2, Backblaze B2, or any other provider.

## Decision

- Puxiri can upload each capture to one S3-compatible destination. It is off unless `--replication-s3-bucket` / `PUXIRI_REPLICATION_S3_BUCKET` is set. The prefix (default `puxiri`), region (default `us-east-1`), endpoint, and path-style addressing are flags with `PUXIRI_REPLICATION_S3_*` environment variables.
- Credentials are read only from `PUXIRI_REPLICATION_S3_ACCESS_KEY_ID` and `PUXIRI_REPLICATION_S3_SECRET_ACCESS_KEY`. There is no flag for them, and the administration panel neither shows nor edits them. Startup fails when the bucket is set and either credential is missing.
- An endpoint must use HTTPS. Plain HTTP is accepted only for a loopback host, for a local test server.
- The remote ID passed to the library is `s3:{endpoint}:{bucket}`, or `s3:aws:{region}:{bucket}` without an endpoint. It never contains credentials.
- Capture stays local first. After the gated local capture, Puxiri calls `sync_to_s3`. An upload failure does not undo the local capture: the response reports `remote.status = failed`, and the next capture retries the idempotent upload.
- `/gestao` shows the destination as `s3://bucket/prefix` and the newest point the destination acknowledged. An acknowledgement written for a different remote ID or prefix is not shown.
- `POST /api/v1/admin/replication/remote/restore` restores the newest remote point into a new file under the data directory's `restores` folder, as the local restore does. It answers 409 `replication_remote_not_configured` without a destination and 503 `replication_remote_unavailable` when the destination cannot be read. It never replaces the live database.

### Scheduled capture and retention (2026-11-12)

- Puxiri captures by itself every `PUXIRI_REPLICATION_INTERVAL_SECONDS` (default 300). `0` leaves capture to an administrator. A scheduled capture that finds another capture or a restore running is skipped, not queued. On shutdown no new capture starts, and one already running finishes before the database closes.
- One capture or restore runs at a time. The others answer 409 `replication_busy`.
- History is kept in generations. The library keeps every level-zero delta of a repository, and its compaction and garbage collection are private and keep those deltas too, so a single repository only grows. When the current generation is older than `PUXIRI_REPLICATION_GENERATION_HOURS` (default 24), the next capture starts a new repository from a fresh snapshot under `generations/{uuidv7}` and switches the `current-generation` pointer atomically. An unfinished generation is removed and never pointed to. If starting a generation fails for a reason other than busy, the capture goes into the current one.
- Locally only the current generation is kept: the new snapshot holds everything the previous one did.
- Remotely each generation uploads to `{prefix}/generations/{id}`. After an acknowledged upload, Puxiri keeps the newest `PUXIRI_REPLICATION_RETAIN_GENERATIONS` (default 7, the current one included) and deletes the rest. It only deletes keys whose segment is the exact UUID form it writes, so other objects under the prefix are left alone. It prunes after a new generation starts, at the first acknowledged upload after startup, and after a failed prune.
- An upload is skipped when the destination already acknowledged the local watermark, so idle scheduled captures do not re-check every object.
- Remote restore uses the newest generation whose manifest was published (the library publishes it last). It falls back to a repository written directly under the prefix before generations existed. Locally, a repository from before generations stays in place untouched, and local restore uses it until the first generation exists.

## Alternatives considered

- Storing S3 settings and credentials in the database through the panel, encrypted with the master key, like the email senders. The master key lives in the data directory, so a server lost together with its disk could not read the credentials needed to recover. Bootstrap configuration outside the data directory survives that loss.
- Uploading from a background worker on a timer. The library's coordinator needs adapter notifications and strict admission control that Puxiri does not wire yet. Uploading right after the administrator's capture keeps one clear boundary.
- Failing the whole capture when the upload fails. The local capture is already durable and useful; failing it would hide that progress.
- Making the library's compaction and garbage collection public and calling them. They keep every level-zero delta, so storage would still grow, and compaction adds a full snapshot on top. Generations bound storage with the library's public API.
- Using the library's coordinator `run` loop for scheduling. It needs adapter notifications and strict admission control Puxiri does not wire; a timer around the same gated capture is enough.

## Consequences and limits

- Only a disposable loopback RustFS 1.0.4 has been exercised from Puxiri (`make test-replication-s3`). AWS S3, R2, and other providers are not certified. The operator should follow `docs/s3-compatible-validation.md` in `sqlite-replication` with a test bucket before relying on a provider.
- The recovery point is the last scheduled or manual capture that reached the destination, so up to one interval of writes, plus any upload that failed.
- Remote storage is bounded by the retained generations: each one holds a full snapshot plus up to one generation age of deltas. Point-in-time restore through the API only takes the newest point; older generations are there for manual recovery.
- Remote restore needs local temporary space proportional to the remote repository.
- No power-loss guarantee. ADR 0002 stays Deferred.
- This supersedes the ADR 0007 statement that Puxiri does not upload the repository or call the library's S3 client. The rest of ADR 0007 stands.

## Evidence

- On 2026-11-09 the operator said the S3 support of `sqlite-replication` had moved from a separate branch to `main` and asked Puxiri to use it.
- `sqlite-replication` `main` at `d708fd2` documents RustFS 1.0.4 and Floci runs and states that real providers are not validated.
- On 2026-11-12 the operator asked for the scheduled capture and the retention limits to be resolved.
- `tests/replication.rs` checks generation rotation, the scheduler and its shutdown, and, against RustFS, that retention keeps two of three generations, leaves foreign keys alone, and still restores every row. It also uploads two captures to a disposable RustFS, then restores both from a different data directory, and checks that an unreachable destination keeps the local capture.
