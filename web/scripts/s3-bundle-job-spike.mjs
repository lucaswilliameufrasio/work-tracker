// Phase 0 supervisor fixture. Lease expiry alone NEVER proves a worker stopped.
// Recovery requires the supervisor to join/terminate that worker before ackStopped.
// This is not an authorized API or an automatic distributed failure detector.
import { randomUUID } from 'node:crypto';
import { lstat, mkdir, mkdtemp, realpath, rm } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { buildBundle } from './s3-bundle-spike.mjs';

export class BundleJobs {
  constructor(store, root) {
    this.store = store;
    this.root = resolve(root);
    store.db.exec(`CREATE TABLE IF NOT EXISTS bundle_jobs (
      id TEXT PRIMARY KEY, directory TEXT NOT NULL UNIQUE,
      state TEXT NOT NULL CHECK (state IN ('pending','running','recovering','complete','incomplete','failed','interrupted')),
      epoch INTEGER NOT NULL DEFAULT 0, worker_epoch INTEGER,
      pin_job TEXT UNIQUE, lease_until INTEGER NOT NULL DEFAULT 0,
      cleanup_pending INTEGER NOT NULL DEFAULT 0 CHECK (cleanup_pending IN (0,1))
    )`);
  }

  async reserve() {
    await mkdir(this.root, { recursive: true, mode: 0o700 });
    const directory = await mkdtemp(join(this.root, 'bundle-run-'));
    const id = randomUUID();
    this.store.db
      .prepare(
        `INSERT INTO bundle_jobs (id, directory, state) VALUES (?, ?, 'pending')`,
      )
      .run(id, directory);
    return id;
  }

  get(id) {
    return this.store.db
      .prepare('SELECT * FROM bundle_jobs WHERE id = ?')
      .get(id);
  }

  claim(id, now, lease) {
    if (!Number.isSafeInteger(lease) || lease <= 0)
      throw new Error('Invalid lease');
    return this.store.db
      .prepare(
        `UPDATE bundle_jobs SET state = 'running', epoch = epoch + 1,
      worker_epoch = epoch + 1, pin_job = 'bundle:' || id || ':' || (epoch + 1), lease_until = ?
      WHERE id = ? AND state = 'pending' RETURNING *`,
      )
      .get(now + lease, id);
  }

  assertCurrent(claim, now) {
    const current = this.get(claim.id);
    if (
      !current ||
      current.state !== 'running' ||
      current.epoch !== claim.epoch ||
      current.lease_until <= now
    ) {
      throw new Error('Bundle worker fenced or expired');
    }
    return current;
  }

  heartbeat(claim, now, lease) {
    if (!Number.isSafeInteger(lease) || lease <= 0)
      throw new Error('Invalid lease');
    return (
      this.store.db
        .prepare(
          `UPDATE bundle_jobs SET lease_until = ?
      WHERE id = ? AND state = 'running' AND epoch = ? AND lease_until > ?`,
        )
        .run(now + lease, claim.id, claim.epoch, now).changes === 1
    );
  }

  finish(claim, state, now) {
    if (!['complete', 'incomplete', 'failed'].includes(state))
      throw new Error('Invalid terminal state');
    this.store.db.exec('BEGIN IMMEDIATE');
    try {
      const row = this.store.db
        .prepare(
          `UPDATE bundle_jobs SET state = ?, lease_until = 0,
        cleanup_pending = CASE WHEN ? = 'incomplete' THEN 0 ELSE 1 END
        WHERE id = ? AND state = 'running' AND epoch = ? AND lease_until > ? RETURNING pin_job`,
        )
        .get(state, state, claim.id, claim.epoch, now);
      if (row) this.store.release(row.pin_job);
      this.store.db.exec('COMMIT');
      return Boolean(row);
    } catch (error) {
      this.store.db.exec('ROLLBACK');
      throw error;
    }
  }

  requestRecovery(id, now) {
    // Fence completion, but retain pins until the original worker is joined.
    return this.store.db
      .prepare(
        `UPDATE bundle_jobs SET state = 'recovering', epoch = epoch + 1
      WHERE id = ? AND state = 'running' AND lease_until <= ? RETURNING *`,
      )
      .get(id, now);
  }

  ackStopped(recovery, stoppedEpoch) {
    this.store.db.exec('BEGIN IMMEDIATE');
    try {
      const row = this.store.db
        .prepare(
          `UPDATE bundle_jobs SET state = 'interrupted',
        lease_until = 0, cleanup_pending = 1 WHERE id = ? AND state = 'recovering'
        AND epoch = ? AND worker_epoch = ? RETURNING pin_job`,
        )
        .get(recovery.id, recovery.epoch, stoppedEpoch);
      if (row) this.store.release(row.pin_job);
      this.store.db.exec('COMMIT');
      return Boolean(row);
    } catch (error) {
      this.store.db.exec('ROLLBACK');
      throw error;
    }
  }

  async cleanup(id, afterRemoval = async () => {}) {
    const row = this.get(id);
    if (
      !row?.cleanup_pending ||
      !['complete', 'failed', 'interrupted'].includes(row.state)
    )
      return false;
    // Paths are owned by this fixture, not supplied by a user or restore manifest.
    // Refuse changed roots/run symlinks instead of following them during cleanup.
    if (
      dirname(row.directory) !== this.root ||
      !basename(row.directory).startsWith('bundle-run-') ||
      (await realpath(this.root)) !== this.root ||
      (await lstat(row.directory)).isSymbolicLink() ||
      (await realpath(row.directory)) !== row.directory
    )
      throw new Error('Unsafe bundle cleanup path');
    await rm(join(row.directory, 'spool'), { recursive: true, force: true });
    await rm(join(row.directory, 'bundle.pxb.partial'), { force: true });
    // Never remove bundle.pxb: it may be a complete-but-unregistered candidate.
    await afterRemoval();
    this.store.db
      .prepare(
        `UPDATE bundle_jobs SET cleanup_pending = 0 WHERE id = ? AND epoch = ?`,
      )
      .run(id, row.epoch);
    return true;
  }
}

export async function runDurableBundle({
  jobs,
  claim,
  clock,
  afterBuilt = async () => {},
  ...options
}) {
  claim = jobs.assertCurrent(claim, clock());
  try {
    const result = await buildBundle({
      ...options,
      store: jobs.store,
      job: claim.pin_job,
      directory: join(claim.directory, 'spool'),
      output: join(claim.directory, 'bundle.pxb'),
      releasePins: false,
      guardFreeze: () => jobs.assertCurrent(claim, clock()),
    });
    await afterBuilt(result);
    if (!jobs.finish(claim, result.status, clock()))
      throw new Error('Bundle worker fenced or expired');
    return result;
  } catch (error) {
    jobs.finish(claim, 'failed', clock());
    throw error;
  }
}
