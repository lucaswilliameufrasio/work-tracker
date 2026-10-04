// Real 4 MiB tmpfs fixture for preflight, not a mocked statfs result.
import assert from 'node:assert/strict';
import {
  mkdtemp,
  readFile,
  rm,
  stat,
  statfs,
  writeFile,
} from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { LifecycleStore } from './s3-lifecycle-spike.mjs';
import { freezeSnapshot } from './s3-bundle-spike.mjs';

const root = await mkdtemp(join(tmpdir(), 'puxiri-space-'));
const path = join(root, 'state.sqlite3');
const store = new LifecycleStore(path);
try {
  const lastComplete = join(root, 'last-valid.pxb');
  await writeFile(lastComplete, 'existing artifact must survive');
  const disk = await statfs(root);
  const freeBytes = disk.bavail * disk.bsize;
  assert.ok(freeBytes < 2 * 8 * 1024 * 1024 + 65536);
  await assert.rejects(
    freezeSnapshot(store, path, 'low-space', join(root, 'spool')),
    /Insufficient free space/,
  );
  assert.equal(store.db.prepare('SELECT count(*) AS n FROM pins').get().n, 0);
  await assert.rejects(
    stat(join(root, 'spool', 'database.sqlite3')),
    (error) => error.code === 'ENOENT',
  );
  assert.equal(
    await readFile(lastComplete, 'utf8'),
    'existing artifact must survive',
  );
  console.log(
    JSON.stringify({
      rejected: true,
      pins: 0,
      freeBytes,
      preservedArtifact: true,
    }),
  );
} finally {
  store.close();
  await rm(root, { recursive: true, force: true });
}
