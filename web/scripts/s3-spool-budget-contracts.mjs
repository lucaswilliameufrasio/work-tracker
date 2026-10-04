import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { stat } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import { freezeSnapshot } from './s3-bundle-spike.mjs';

export function registerSpoolBudgetTests({ test, lifecycleFixture }) {
  const run = promisify(execFile);
  test('inventory/byte preflight bounds allocations and rolls back before pins or snapshot copying', async (t) => {
    const fixture = await lifecycleFixture(t);
    const root = dirname(fixture.path);
    fixture.store.db
      .prepare(
        `INSERT INTO objects (key, bytes, sha256, state)
      VALUES ('images/objects/one', 1, 'unused', 'available'), ('images/objects/two', 1, 'unused', 'available')`,
      )
      .run();
    fixture.store.db
      .prepare(
        `INSERT INTO refs (owner, object_key)
      VALUES ('one', 'images/objects/one'), ('two', 'images/objects/two')`,
      )
      .run();
    for (const [limits, message] of [
      [
        { maxObjects: 0, maxBytes: 8 * 1024 * 1024 },
        /Invalid bundle fixture limits/,
      ],
      [{ maxObjects: 2, maxBytes: 1 }, /safety budget/],
      [
        { maxObjects: 1, maxBytes: 8 * 1024 * 1024 },
        /inventory exceeds fixture limit/,
      ],
    ]) {
      await assert.rejects(
        freezeSnapshot(
          fixture.store,
          fixture.path,
          'budget',
          join(root, 'preflight'),
          undefined,
          undefined,
          limits,
        ),
        message,
      );
      assert.equal(
        fixture.store.db.prepare('SELECT count(*) AS n FROM pins').get().n,
        0,
      );
    }
    await assert.rejects(
      stat(join(root, 'preflight', 'database.sqlite3')),
      (error) => error.code === 'ENOENT',
    );
    // The original upload is pending: this write proves the guard lock rolled back.
    assert.equal(fixture.store.cancel(fixture.id), true);
  });

  test('real low-space filesystem rejects bundle preflight without deleting valid artifacts', async () => {
    const name = `puxiri-space-${randomUUID()}`;
    const root = fileURLToPath(new URL('../../', import.meta.url));
    try {
      const { stdout } = await run(
        'docker',
        [
          'run',
          '--rm',
          '--name',
          name,
          '--label',
          'puxiri.test=spool-space',
          '--network',
          'none',
          '--read-only',
          '--cap-drop',
          'ALL',
          '--security-opt',
          'no-new-privileges',
          '--pids-limit',
          '32',
          '--memory',
          '128m',
          '--memory-swap',
          '128m',
          '--user',
          `${process.getuid()}:${process.getgid()}`,
          '--tmpfs',
          '/scratch:rw,size=4m,mode=1777',
          '--env',
          'TMPDIR=/scratch',
          '--mount',
          `type=bind,src=${root},dst=/app,readonly`,
          '--entrypoint',
          'node',
          'node@sha256:0e0ff40c39bc087845bfb27465a0df4ea419520094bc35842ff83dd8cbe6f9b6',
          '/app/web/scripts/s3-spool-space-worker.mjs',
        ],
        { timeout: 30000, maxBuffer: 1024 * 1024 },
      );
      const report = JSON.parse(stdout);
      assert.equal(report.rejected, true);
      assert.equal(report.pins, 0);
      assert.equal(report.preservedArtifact, true);
      assert.ok(report.freeBytes <= 4 * 1024 * 1024);
    } finally {
      await run('docker', ['rm', '-f', name], { timeout: 10000 }).catch(
        (error) => {
          if (!error.stderr?.includes('No such container')) throw error;
        },
      );
    }
  });
}
