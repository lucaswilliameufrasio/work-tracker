// Phase 0 fixture schema, not production migrations or an authorized API.
// Real file-backed SQLite coordinates durable state with real remote S3 effects.
import { DatabaseSync } from 'node:sqlite';
import { inspectStaging, promoteValidated } from './s3-confirmation-spike.mjs';

export class LifecycleStore {
  constructor(path) {
    this.db = new DatabaseSync(path);
    this.db.exec(`
      PRAGMA journal_mode = WAL;
      PRAGMA synchronous = FULL;
      PRAGMA foreign_keys = ON;
      PRAGMA busy_timeout = 5000;
      CREATE TABLE IF NOT EXISTS uploads (
        id TEXT PRIMARY KEY, staging_key TEXT NOT NULL UNIQUE,
        final_key TEXT NOT NULL UNIQUE, sha256 TEXT NOT NULL,
        bytes INTEGER NOT NULL CHECK (bytes >= 0),
        state TEXT NOT NULL CHECK (state IN ('pending','confirming','available','cancelled')),
        epoch INTEGER NOT NULL DEFAULT 0, lease_until INTEGER NOT NULL DEFAULT 0
      );
      CREATE TABLE IF NOT EXISTS objects (
        key TEXT PRIMARY KEY, sha256 TEXT NOT NULL, bytes INTEGER NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('available','deleting','deleted'))
      );
      CREATE TABLE IF NOT EXISTS refs (
        owner TEXT PRIMARY KEY, object_key TEXT NOT NULL REFERENCES objects(key)
      );
      CREATE TABLE IF NOT EXISTS pins (
        job TEXT NOT NULL, object_key TEXT NOT NULL REFERENCES objects(key),
        PRIMARY KEY (job, object_key)
      );
    `);
  }

  close() {
    this.db.close();
  }

  reserve({ id, stagingKey, finalKey, sha256, bytes }) {
    this.db
      .prepare(
        `INSERT INTO uploads (id, staging_key, final_key, sha256, bytes, state)
      VALUES (?, ?, ?, ?, ?, 'pending')`,
      )
      .run(id, stagingKey, finalKey, sha256, bytes);
  }

  claim(id, now, lease) {
    return this.db
      .prepare(
        `UPDATE uploads SET state = 'confirming', epoch = epoch + 1,
      lease_until = ? WHERE id = ? AND (state = 'pending' OR
        (state = 'confirming' AND lease_until <= ?)) RETURNING *`,
      )
      .get(now + lease, id, now);
  }

  cancel(id) {
    return (
      this.db
        .prepare(
          `UPDATE uploads SET state = 'cancelled', epoch = epoch + 1,
      lease_until = 0 WHERE id = ? AND state IN ('pending','confirming')`,
        )
        .run(id).changes === 1
    );
  }

  complete(claim, verified, now) {
    this.db.exec('BEGIN IMMEDIATE');
    try {
      const current = this.db
        .prepare(
          `UPDATE uploads SET state = 'available', lease_until = 0
        WHERE id = ? AND state = 'confirming' AND epoch = ? AND lease_until > ? RETURNING *`,
        )
        .get(claim.id, claim.epoch, now);
      if (!current) {
        this.db.exec('ROLLBACK');
        return false;
      }
      if (
        verified.key !== current.final_key ||
        verified.bytes !== current.bytes ||
        verified.sha256 !== current.sha256
      ) {
        throw new Error('Remote evidence does not match the reserved object');
      }
      this.db
        .prepare(
          `INSERT INTO objects (key, sha256, bytes, state)
        VALUES (?, ?, ?, 'available')`,
        )
        .run(current.final_key, current.sha256, current.bytes);
      this.db
        .prepare('INSERT INTO refs (owner, object_key) VALUES (?, ?)')
        .run(current.id, current.final_key);
      this.db.exec('COMMIT');
      return true;
    } catch (error) {
      this.db.exec('ROLLBACK');
      throw error;
    }
  }

  available(id) {
    return this.db
      .prepare(
        `SELECT objects.key FROM uploads JOIN objects ON objects.key = uploads.final_key
      WHERE uploads.id = ? AND uploads.state = 'available' AND objects.state = 'available'`,
      )
      .get(id);
  }

  objectCount() {
    return this.db.prepare('SELECT count(*) AS count FROM objects').get().count;
  }

  unreference(key) {
    this.db.prepare('DELETE FROM refs WHERE object_key = ?').run(key);
  }

  pin(job, key) {
    return (
      this.db
        .prepare(
          `INSERT INTO pins (job, object_key)
      SELECT ?, key FROM objects WHERE key = ? AND state = 'available'`,
        )
        .run(job, key).changes === 1
    );
  }

  release(job) {
    this.db.prepare('DELETE FROM pins WHERE job = ?').run(job);
  }

  claimGc(key) {
    return this.db
      .prepare(
        `UPDATE objects SET state = 'deleting' WHERE key = ? AND state = 'available'
      AND NOT EXISTS (SELECT 1 FROM refs WHERE object_key = objects.key)
      AND NOT EXISTS (SELECT 1 FROM pins WHERE object_key = objects.key) RETURNING key`,
      )
      .get(key);
  }

  deleted(key) {
    return (
      this.db
        .prepare(
          "UPDATE objects SET state = 'deleted' WHERE key = ? AND state = 'deleting'",
        )
        .run(key).changes === 1
    );
  }

  deleting() {
    return this.db
      .prepare("SELECT key FROM objects WHERE state = 'deleting'")
      .all();
  }
}

// This prepares/verifies remote effects. The caller must commit availability
// with the same current claim afterwards; remote PUT and SQLite are not atomic.
export async function prepareRemote(client, bucket, claim) {
  const expected = { maxBytes: claim.bytes, sha256: claim.sha256 };
  try {
    // Recovery first: an earlier worker may have finished PUT before crashing.
    // Read all bytes; HEAD, mere presence, ETag or HTTP 200 are not sufficient.
    return await inspectStaging(client, bucket, claim.final_key, expected);
  } catch (error) {
    if (error.$metadata?.httpStatusCode !== 404) throw error;
  }
  const validated = await inspectStaging(
    client,
    bucket,
    claim.staging_key,
    expected,
  );
  try {
    return await promoteValidated(client, bucket, validated, claim.final_key);
  } catch (error) {
    if (error.$metadata?.httpStatusCode !== 412) throw error;
    // Another worker may have won a create-only PUT. Never overwrite; verify
    // that exact deterministic final key before any attempt to commit state.
    return inspectStaging(client, bucket, claim.final_key, expected);
  }
}
