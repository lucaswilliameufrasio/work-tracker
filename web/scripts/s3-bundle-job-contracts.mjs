import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import { once } from 'node:events';
import { randomBytes } from 'node:crypto';
import {
  mkdir,
  readFile,
  rmdir,
  stat,
  symlink,
  unlink,
  writeFile,
} from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DeleteObjectCommand } from '@aws-sdk/client-s3';
import { prepareRemote } from './s3-lifecycle-spike.mjs';
import { BundleJobs, runDurableBundle } from './s3-bundle-job-spike.mjs';
import { restoreIsolated } from './s3-bundle-spike.mjs';

export function registerBundleJobTests({ test, lifecycleFixture, context }) {
  async function fixtureFor(t) {
    const { client, bucket } = context();
    const fixture = await lifecycleFixture(t);
    const claim = fixture.store.claim(fixture.id, 100, 10);
    assert.equal(
      fixture.store.complete(
        claim,
        await prepareRemote(client, bucket, claim),
        101,
      ),
      true,
    );
    const root = join(dirname(fixture.path), 'bundle-jobs');
    const jobs = new BundleJobs(fixture.store, root);
    return { fixture, root, jobs };
  }

  test('bundle SIGKILL at four boundaries retains pins until explicit joined-worker recovery', async (t) => {
    for (const point of ['pins', 'snapshot', 'partial', 'published']) {
      await t.test(point, async (t) => {
        const { client, bucket, endpoint, credentials } = context();
        const { fixture, root, jobs } = await fixtureFor(t);
        const key = randomBytes(32);
        // A genuinely restorable prior artifact, in a different durable run.
        const previousId = await jobs.reserve();
        const previousClaim = jobs.claim(previousId, 100, 100);
        const previous = await runDurableBundle({
          jobs,
          claim: previousClaim,
          clock: () => 101,
          sourcePath: fixture.path,
          client,
          bucket,
          key,
          keyId: 'job-fixture-key',
        });
        const previousBytes = await readFile(previous.output);
        const id = await jobs.reserve();
        const claim = jobs.claim(id, 100, 10);
        assert.equal(jobs.claim(id, 105, 10), undefined);
        const worker = fork(
          fileURLToPath(
            new URL('./s3-bundle-interruption-worker.mjs', import.meta.url),
          ),
          [],
          { execArgv: [], stdio: ['ignore', 'ignore', 'ignore', 'ipc'] },
        );
        const exited = once(worker, 'exit');
        let recovery;
        try {
          worker.send({
            path: fixture.path,
            root,
            claim,
            config: { endpoint, region: 'us-east-1', credentials, bucket },
            key: [...key],
            point,
          });
          const [message] = await once(worker, 'message', {
            signal: AbortSignal.timeout(15000),
          });
          assert.equal(message.kind, 'boundary');
          fixture.store.unreference(fixture.finalKey);
          assert.equal(fixture.store.claimGc(fixture.finalKey), undefined);
          // Expiration fences the still-alive child but cannot release its pins.
          assert.equal(jobs.requestRecovery(id, 109), undefined);
          recovery = jobs.requestRecovery(id, 110);
          assert.equal(recovery.state, 'recovering');
          assert.equal(jobs.finish(claim, 'complete', 111), false);
          assert.equal(jobs.heartbeat(claim, 111, 100), false);
          assert.equal(jobs.ackStopped(recovery, claim.epoch + 1), false);
          assert.equal(await jobs.cleanup(id), false);
          assert.equal(fixture.store.claimGc(fixture.finalKey), undefined);
          worker.kill('SIGKILL');
          const [code, signal] = await exited;
          assert.equal(code, null);
          assert.equal(signal, 'SIGKILL');
        } finally {
          if (worker.exitCode === null && worker.signalCode === null) {
            worker.kill('SIGKILL');
            await exited;
          }
        }
        fixture.reopen();
        let reopened = new BundleJobs(fixture.store, root);
        assert.equal(reopened.get(id).state, 'recovering');
        assert.equal(fixture.store.claimGc(fixture.finalKey), undefined);
        // Supervisor has joined the exact child. This evidence is external to
        // SQLite; ackStopped is never an expiry-only automatic GC operation.
        assert.equal(reopened.ackStopped(recovery, claim.epoch), true);
        assert.equal(reopened.ackStopped(recovery, claim.epoch), false);
        assert.equal(reopened.get(id).state, 'interrupted');
        assert.equal(
          fixture.store.claimGc(fixture.finalKey).key,
          fixture.finalKey,
        );
        if (point === 'partial')
          assert.ok(
            (await stat(join(claim.directory, 'bundle.pxb.partial'))).size > 0,
          );
        // Simulate cleanup succeeding on disk but crashing before its DB ack.
        await assert.rejects(
          reopened.cleanup(id, async () => {
            throw new Error('Cleanup interrupted');
          }),
          /Cleanup interrupted/,
        );
        assert.equal(reopened.get(id).cleanup_pending, 1);
        fixture.reopen();
        reopened = new BundleJobs(fixture.store, root);
        assert.equal(reopened.get(id).cleanup_pending, 1);
        assert.equal(await reopened.cleanup(id), true);
        assert.equal(await reopened.cleanup(id), false);
        await assert.rejects(
          stat(join(claim.directory, 'spool')),
          (error) => error.code === 'ENOENT',
        );
        await assert.rejects(
          stat(join(claim.directory, 'bundle.pxb.partial')),
          (error) => error.code === 'ENOENT',
        );
        if (point === 'published') {
          // Candidate is physically complete, but deliberately NOT catalogued
          // complete: recovery must not infer success merely from its existence.
          await restoreIsolated(
            join(claim.directory, 'bundle.pxb'),
            key,
            'job-fixture-key',
            dirname(fixture.path),
          );
          assert.equal(reopened.get(id).state, 'interrupted');
        }
        await client.send(
          new DeleteObjectCommand({ Bucket: bucket, Key: fixture.finalKey }),
        );
        assert.deepEqual(await readFile(previous.output), previousBytes);
        await restoreIsolated(
          previous.output,
          key,
          'job-fixture-key',
          dirname(fixture.path),
        );
        assert.equal(reopened.claim(id, 200, 10), undefined);
      });
    }
  });

  test('bundle jobs persist completion and incomplete diagnostics with atomic terminal pin release', async (t) => {
    const { client, bucket } = context();
    const { fixture, root, jobs } = await fixtureFor(t);
    const key = randomBytes(32);
    const id = await jobs.reserve();
    const claim = jobs.claim(id, 100, 10);
    assert.equal(jobs.heartbeat(claim, 105, 20), true);
    const complete = await runDurableBundle({
      jobs,
      claim,
      clock: () => 111,
      sourcePath: fixture.path,
      client,
      bucket,
      key,
      keyId: 'normal-key',
      afterSnapshot() {
        fixture.store.db
          .exec(`CREATE TRIGGER fail_pin_release BEFORE DELETE ON pins
          BEGIN SELECT RAISE(ABORT, 'Injected pin release failure'); END`);
        try {
          assert.throws(
            () => jobs.finish(claim, 'complete', 111),
            /Injected pin release failure/,
          );
          assert.equal(jobs.get(id).state, 'running');
          assert.equal(
            fixture.store.db
              .prepare('SELECT count(*) AS n FROM pins WHERE job = ?')
              .get(claim.pin_job).n,
            1,
          );
        } finally {
          fixture.store.db.exec('DROP TRIGGER fail_pin_release');
        }
      },
    });
    assert.equal(complete.status, 'complete');
    assert.equal(jobs.get(id).state, 'complete');
    assert.equal(jobs.finish(claim, 'complete', 112), false);
    assert.equal(await jobs.cleanup(id), true);
    await restoreIsolated(
      complete.output,
      key,
      'normal-key',
      dirname(fixture.path),
    );
    await client.send(
      new DeleteObjectCommand({ Bucket: bucket, Key: fixture.finalKey }),
    );
    const incompleteId = await jobs.reserve();
    const incompleteClaim = jobs.claim(incompleteId, 100, 10);
    const result = await runDurableBundle({
      jobs,
      claim: incompleteClaim,
      clock: () => 101,
      sourcePath: fixture.path,
      client,
      bucket,
      key,
      keyId: 'normal-key',
    });
    assert.equal(result.status, 'incomplete');
    fixture.reopen();
    const reopened = new BundleJobs(fixture.store, root);
    assert.equal(reopened.get(incompleteId).state, 'incomplete');
    assert.equal(await reopened.cleanup(incompleteId), false);
    assert.ok(
      (await stat(join(incompleteClaim.directory, 'spool', 'incomplete.json')))
        .size > 0,
    );
    assert.equal(
      fixture.store.db.prepare('SELECT count(*) AS n FROM pins').get().n,
      0,
    );
  });

  test('expired bundle claims cannot freeze or commit and cleanup refuses run-directory symlinks', async (t) => {
    const { client, bucket } = context();
    const { fixture, root, jobs } = await fixtureFor(t);
    const id = await jobs.reserve();
    assert.throws(() => jobs.claim(id, 100, 0), /Invalid lease/);
    const claim = jobs.claim(id, 100, 10);
    await assert.rejects(
      runDurableBundle({
        jobs,
        claim,
        clock: () => 110,
        sourcePath: fixture.path,
        client,
        bucket,
        key: randomBytes(32),
        keyId: 'expired',
      }),
      /expired/,
    );
    assert.equal(
      fixture.store.db.prepare('SELECT count(*) AS n FROM pins').get().n,
      0,
    );
    const recovery = jobs.requestRecovery(id, 110);
    // No child was ever started, so the supervisor knows this epoch is stopped.
    assert.equal(jobs.ackStopped(recovery, claim.epoch), true);
    const outside = join(dirname(fixture.path), 'unrelated');
    await mkdir(join(outside, 'spool'), { recursive: true });
    await writeFile(join(outside, 'spool', 'keep'), 'unrelated data');
    // Remove only the fixture's empty reservation directory, then substitute link.
    await rmdir(claim.directory);
    await symlink(outside, claim.directory);
    await assert.rejects(jobs.cleanup(id), /Unsafe bundle cleanup path/);
    assert.equal(
      await readFile(join(outside, 'spool', 'keep'), 'utf8'),
      'unrelated data',
    );
    await unlink(claim.directory);
    await mkdir(claim.directory);
    assert.equal(await jobs.cleanup(id), true);
    assert.equal(jobs.get(id).cleanup_pending, 0);
    assert.equal(root, dirname(claim.directory));
  });

  test('bundle errors become terminal atomically, but lease loss after publication requires stopped-worker recovery', async (t) => {
    const { client, bucket } = context();
    const { fixture, jobs } = await fixtureFor(t);
    const failedId = await jobs.reserve();
    const failedClaim = jobs.claim(failedId, 100, 10);
    await assert.rejects(
      runDurableBundle({
        jobs,
        claim: failedClaim,
        clock: () => 101,
        sourcePath: fixture.path,
        client,
        bucket,
        key: randomBytes(32),
        keyId: 'failure',
        afterSnapshot() {
          throw new Error('Snapshot processing interrupted');
        },
      }),
      /Snapshot processing interrupted/,
    );
    assert.equal(jobs.get(failedId).state, 'failed');
    assert.equal(
      fixture.store.db.prepare('SELECT count(*) AS n FROM pins').get().n,
      0,
    );
    assert.equal(await jobs.cleanup(failedId), true);
    const id = await jobs.reserve();
    const claim = jobs.claim(id, 100, 10);
    let now = 101;
    const key = randomBytes(32);
    await assert.rejects(
      runDurableBundle({
        jobs,
        claim: { ...claim, directory: '/not-the-canonical-directory' },
        clock: () => now,
        sourcePath: fixture.path,
        client,
        bucket,
        key,
        keyId: 'expired-after-publication',
        afterBuilt() {
          now = 110;
        },
      }),
      /fenced or expired/,
    );
    assert.equal(jobs.get(id).state, 'running');
    assert.equal(
      fixture.store.db
        .prepare('SELECT count(*) AS n FROM pins WHERE job = ?')
        .get(claim.pin_job).n,
      1,
    );
    fixture.store.unreference(fixture.finalKey);
    assert.equal(fixture.store.claimGc(fixture.finalKey), undefined);
    const recovery = jobs.requestRecovery(id, 110);
    // Synchronous caller has joined the rejected promise; no live worker remains.
    assert.equal(jobs.ackStopped(recovery, claim.epoch), true);
    assert.equal(await jobs.cleanup(id), true);
    assert.equal(fixture.store.claimGc(fixture.finalKey).key, fixture.finalKey);
    await restoreIsolated(
      join(claim.directory, 'bundle.pxb'),
      key,
      'expired-after-publication',
      dirname(fixture.path),
    );
    assert.equal(jobs.get(id).state, 'interrupted');
  });
}
