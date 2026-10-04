import assert from 'node:assert/strict';
import { randomBytes, randomUUID, createHash } from 'node:crypto';
import { readFile, stat, writeFile } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { dirname, join } from 'node:path';
import { Readable } from 'node:stream';
import { Header, c as pack } from 'tar';
import {
  CreateBucketCommand,
  DeleteBucketCommand,
  DeleteObjectCommand,
  HeadObjectCommand,
  PutObjectCommand,
} from '@aws-sdk/client-s3';
import {
  buildBundle,
  restoreIsolated,
  sealArchive,
} from './s3-bundle-spike.mjs';
import { prepareRemote } from './s3-lifecycle-spike.mjs';

// Registered in the existing suite to use its isolated real MinIO lifecycle.
export function registerBundleTests({ test, lifecycleFixture, context }) {
  async function availableFixture(t) {
    const { client, bucket } = context();
    const fixture = await lifecycleFixture(t);
    const claim = fixture.store.claim(fixture.id, 100, 10);
    const verified = await prepareRemote(client, bucket, claim);
    assert.equal(fixture.store.complete(claim, verified, 101), true);
    return fixture;
  }

  test('encrypted self-contained bundle restores snapshot images/avatar after source bytes are deleted', async (t) => {
    const { client, bucket, signedPut, signedGet, http } = context();
    const fixture = await availableFixture(t);
    const id = randomUUID();
    const avatar = Buffer.from('snapshot private avatar');
    const avatarKey = `avatars/objects/${id}`;
    const avatarStaging = `avatars/_uploads/${id}`;
    fixture.store.reserve({
      id,
      stagingKey: avatarStaging,
      finalKey: avatarKey,
      bytes: avatar.length,
      sha256: createHash('sha256').update(avatar).digest('base64'),
    });
    const put = await signedPut(avatarStaging, avatar);
    assert.equal(
      (
        await http(put.url, {
          method: 'PUT',
          headers: put.headers,
          body: avatar,
        })
      ).status,
      200,
    );
    const claim = fixture.store.claim(id, 100, 10);
    assert.equal(
      fixture.store.complete(
        claim,
        await prepareRemote(client, bucket, claim),
        101,
      ),
      true,
    );
    const exportKey = `exports/${randomUUID()}`;
    const sourceBucket = `puxiri-source-${randomUUID()}`;
    await client.send(new CreateBucketCommand({ Bucket: sourceBucket }));
    for (const [remote, body] of [
      [fixture.finalKey, fixture.body],
      [avatarKey, avatar],
      [exportKey, Buffer.from('excluded export')],
    ]) {
      await client.send(
        new PutObjectCommand({ Bucket: sourceBucket, Key: remote, Body: body }),
      );
    }
    const root = dirname(fixture.path);
    const key = randomBytes(32);
    const output = join(root, 'complete.pxb');
    const built = await buildBundle({
      store: fixture.store,
      sourcePath: fixture.path,
      client,
      bucket: sourceBucket,
      job: 'snapshot-files',
      directory: join(root, 'snapshot'),
      output,
      key,
      keyId: 'fixture-key-v1',
      afterPinned(inventory) {
        assert.equal(inventory.length, 2);
        for (const object of inventory) {
          fixture.store.unreference(object.key);
          assert.equal(fixture.store.claimGc(object.key), undefined);
        }
      },
    });
    assert.equal(built.status, 'complete');
    assert.ok(built.bytes > 0);
    assert.equal(built.manifest.files.length, 2);
    assert.equal(
      fixture.store.db.prepare('SELECT count(*) AS n FROM refs').get().n,
      0,
    );
    const backupKey = `backups/${randomUUID()}`;
    await client.send(
      new PutObjectCommand({
        Bucket: bucket,
        Key: backupKey,
        Body: createReadStream(output),
        ContentLength: built.bytes,
        IfNoneMatch: '*',
      }),
    );
    const downloaded = await http(await signedGet(backupKey, 3600));
    assert.equal(downloaded.status, 200);
    const local = join(root, 'downloaded.pxb');
    await writeFile(local, downloaded.body, { flag: 'wx', mode: 0o600 });
    for (const remote of [fixture.finalKey, avatarKey]) {
      assert.equal(fixture.store.claimGc(remote).key, remote);
      await client.send(
        new DeleteObjectCommand({ Bucket: sourceBucket, Key: remote }),
      );
      await client.send(
        new DeleteObjectCommand({ Bucket: bucket, Key: remote }),
      );
      assert.equal(fixture.store.deleted(remote), true);
      await assert.rejects(
        client.send(new HeadObjectCommand({ Bucket: bucket, Key: remote })),
        (error) => error.$metadata?.httpStatusCode === 404,
      );
    }
    for (const remote of [fixture.stagingKey, avatarStaging]) {
      await client.send(
        new DeleteObjectCommand({ Bucket: bucket, Key: remote }),
      );
    }
    await client.send(
      new DeleteObjectCommand({ Bucket: sourceBucket, Key: exportKey }),
    );
    await client.send(new DeleteBucketCommand({ Bucket: sourceBucket }));
    // Restore receives no client, endpoint or credentials: local bytes and key only.
    const restored = await restoreIsolated(local, key, 'fixture-key-v1', root);
    const expected = new Map([
      [fixture.finalKey, fixture.body],
      [avatarKey, avatar],
    ]);
    for (const object of restored.manifest.files) {
      assert.deepEqual(
        await readFile(join(restored.extracted, object.path)),
        expected.get(object.key),
      );
    }
  });

  test('missing remote bytes preserve an incomplete snapshot and never replace the last complete bundle', async (t) => {
    const { client, bucket } = context();
    const fixture = await availableFixture(t);
    const root = dirname(fixture.path);
    const key = randomBytes(32);
    const output = join(root, 'last-valid.pxb');
    const options = {
      store: fixture.store,
      sourcePath: fixture.path,
      client,
      bucket,
      output,
      key,
      keyId: 'offline-key',
    };
    const complete = await buildBundle({
      ...options,
      job: 'complete',
      directory: join(root, 'valid-snapshot'),
    });
    assert.equal(complete.status, 'complete');
    const previous = await readFile(output);
    await client.send(
      new DeleteObjectCommand({ Bucket: bucket, Key: fixture.finalKey }),
    );
    const failed = await buildBundle({
      ...options,
      job: 'incomplete',
      directory: join(root, 'incomplete-snapshot'),
    });
    assert.equal(failed.status, 'incomplete');
    assert.deepEqual(failed.missing, [
      { key: fixture.finalKey, reason: 'missing' },
    ]);
    assert.ok(
      (await stat(join(failed.directory, 'database.sqlite3'))).size > 0,
    );
    assert.deepEqual(await readFile(output), previous);
    assert.equal(
      fixture.store.db.prepare('SELECT count(*) AS n FROM pins').get().n,
      0,
    );
    await restoreIsolated(output, key, 'offline-key', root);
  });

  test('changed remote bytes cannot publish a complete bundle and release terminal pins', async (t) => {
    const { client, bucket } = context();
    const fixture = await availableFixture(t);
    const root = dirname(fixture.path);
    const output = join(root, 'changed.pxb');
    await client.send(
      new PutObjectCommand({
        Bucket: bucket,
        Key: fixture.finalKey,
        Body: Buffer.alloc(fixture.body.length, 42),
      }),
    );
    const result = await buildBundle({
      store: fixture.store,
      sourcePath: fixture.path,
      client,
      bucket,
      job: 'changed',
      directory: join(root, 'snapshot'),
      output,
      key: randomBytes(32),
      keyId: 'local-key',
    });
    assert.equal(result.status, 'incomplete');
    assert.deepEqual(result.missing, [
      { key: fixture.finalKey, reason: 'unverified' },
    ]);
    await assert.rejects(stat(output), (error) => error.code === 'ENOENT');
    assert.equal(
      fixture.store.db.prepare('SELECT count(*) AS n FROM pins').get().n,
      0,
    );
  });

  test('wrong key, key id, ciphertext corruption and interrupted bundle fail before restore exposure', async (t) => {
    const { client, bucket } = context();
    const fixture = await availableFixture(t);
    const root = dirname(fixture.path);
    const key = randomBytes(32);
    const output = join(root, 'auth.pxb');
    await buildBundle({
      store: fixture.store,
      sourcePath: fixture.path,
      client,
      bucket,
      job: 'authenticated',
      directory: join(root, 'snapshot'),
      output,
      key,
      keyId: 'current-key',
    });
    await assert.rejects(
      restoreIsolated(output, randomBytes(32), 'current-key', root),
    );
    await assert.rejects(
      restoreIsolated(output, key, 'unknown-key', root),
      /key id/,
    );
    const data = await readFile(output);
    const changed = Buffer.from(data);
    changed[Math.floor(changed.length / 2)] ^= 1;
    const corrupt = join(root, 'corrupt.pxb');
    await writeFile(corrupt, changed);
    await assert.rejects(restoreIsolated(corrupt, key, 'current-key', root));
    const interrupted = join(root, 'interrupted.pxb');
    await writeFile(interrupted, data.subarray(0, Math.floor(data.length / 2)));
    await assert.rejects(
      restoreIsolated(interrupted, key, 'current-key', root),
    );
    assert.deepEqual(await readFile(output), data);
  });

  test('authenticated malicious archive entries cannot escape isolated restore', async (t) => {
    const fixture = await lifecycleFixture(t);
    const root = dirname(fixture.path);
    const key = randomBytes(32);
    for (const [type, path, linkpath] of [
      ['File', '../escaped', undefined],
      ['File', join(root, 'escaped'), undefined],
      ['SymbolicLink', 'objects/0.bin', '../outside'],
      ['Link', 'objects/0.bin', '../outside'],
    ]) {
      const header = new Header({
        path,
        type,
        linkpath,
        size: type === 'File' ? 4 : 0,
      });
      header.encode();
      const content =
        type === 'File'
          ? Buffer.concat([Buffer.from('evil'), Buffer.alloc(508)])
          : Buffer.alloc(0);
      const output = join(root, `hostile-${randomUUID()}.pxb`);
      await sealArchive(
        Readable.from([header.block, content, Buffer.alloc(1024)]),
        output,
        key,
        'hostile-key',
      );
      await assert.rejects(
        restoreIsolated(output, key, 'hostile-key', root),
        /forbidden|TAR_ENTRY_ERROR/,
      );
    }
    await assert.rejects(
      stat(join(root, 'escaped')),
      (error) => error.code === 'ENOENT',
    );
  });

  test('authenticated but inconsistent manifests are rejected by inventory/checksum validation', async (t) => {
    const { client, bucket } = context();
    const fixture = await availableFixture(t);
    const root = dirname(fixture.path);
    const key = randomBytes(32);
    const directory = join(root, 'snapshot');
    const built = await buildBundle({
      store: fixture.store,
      sourcePath: fixture.path,
      client,
      bucket,
      job: 'manifest',
      directory,
      output: join(root, 'valid.pxb'),
      key,
      keyId: 'manifest-key',
    });
    assert.equal(built.status, 'complete');
    const paths = [
      'database.sqlite3',
      'manifest.json',
      ...built.manifest.files.map((object) => object.path),
    ];
    for (const [manifest, message] of [
      [
        {
          ...built.manifest,
          database: { ...built.manifest.database, sha256: 'wrong digest' },
        },
        /checksum mismatch/,
      ],
      [{ ...built.manifest, files: [] }, /complete inventory/],
      [
        {
          ...built.manifest,
          files: [{ ...built.manifest.files[0], path: '../escaped' }],
        },
        /manifest paths/,
      ],
    ]) {
      await writeFile(
        join(directory, 'manifest.json'),
        JSON.stringify(manifest),
      );
      const output = join(root, `inconsistent-${randomUUID()}.pxb`);
      await sealArchive(
        pack({ cwd: directory, portable: true, strict: true }, paths),
        output,
        key,
        'manifest-key',
      );
      await assert.rejects(
        restoreIsolated(output, key, 'manifest-key', root),
        message,
      );
    }
  });

  test('interrupted or oversized archive construction leaves no publication and never replaces an artifact', async (t) => {
    const fixture = await lifecycleFixture(t);
    const root = dirname(fixture.path);
    const key = randomBytes(32);
    const output = join(root, 'exclusive.pxb');
    await sealArchive(
      Readable.from([Buffer.from('first test archive')]),
      output,
      key,
      'local-key',
    );
    const original = await readFile(output);
    await assert.rejects(
      sealArchive(
        Readable.from([Buffer.from('replacement')]),
        output,
        key,
        'local-key',
      ),
      (error) => error.code === 'EEXIST',
    );
    assert.deepEqual(await readFile(output), original);
    const interrupted = join(root, 'interrupted-build.pxb');
    await assert.rejects(
      sealArchive(
        Readable.from(
          (async function* () {
            yield Buffer.from('partial');
            throw new Error('Interrupted archive input');
          })(),
        ),
        interrupted,
        key,
        'local-key',
      ),
      /Interrupted archive input/,
    );
    const oversized = join(root, 'oversized-build.pxb');
    await assert.rejects(
      sealArchive(
        Readable.from([Buffer.alloc(8 * 1024 * 1024 + 1)]),
        oversized,
        key,
        'local-key',
      ),
      /safety budget/,
    );
    for (const path of [
      interrupted,
      `${interrupted}.partial`,
      oversized,
      `${oversized}.partial`,
      `${output}.partial`,
    ]) {
      await assert.rejects(stat(path), (error) => error.code === 'ENOENT');
    }
  });
}
