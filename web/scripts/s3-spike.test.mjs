// Phase 0: real, disposable MinIO and Chromium. No production storage adapter.
// No operator credentials/endpoints: all configuration is generated locally.
import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { execFile, fork } from 'node:child_process';
import { once } from 'node:events';
import { promisify } from 'node:util';
import { createServer } from 'node:http';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { chromium } from '@playwright/test';
import {
  CopyObjectCommand,
  CreateBucketCommand,
  DeleteObjectCommand,
  GetObjectCommand,
  HeadObjectCommand,
  ListObjectsV2Command,
  PutObjectCommand,
  S3Client,
} from '@aws-sdk/client-s3';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';
import { inspectStaging, promoteValidated } from './s3-confirmation-spike.mjs';
import { LifecycleStore, prepareRemote } from './s3-lifecycle-spike.mjs';
import { registerBundleTests } from './s3-bundle-contracts.mjs';
import { registerBundleJobTests } from './s3-bundle-job-contracts.mjs';

const image =
  'cgr.dev/chainguard/minio@sha256:dc8b7ba9bdd982d28af533121e5438239624460432e090a52f81b36e7bd5f7dd';
const run = promisify(execFile);
const bucket = `puxiri-spike-${randomUUID()}`;
const credentials = {
  accessKeyId: `spike${randomBytes(8).toString('hex')}`,
  secretAccessKey: randomBytes(32).toString('hex'),
};
const prefixes = ['images', 'avatars', 'backups', 'exports'];
let container;
let endpoint;
let client;
let browser;
let allowedSite;
let deniedSite;

async function site() {
  const server = createServer((_request, response) => {
    response.writeHead(200, { 'content-type': 'text/html' });
    response.end('<!doctype html><title>S3 spike</title>Local browser origin');
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return { server, origin: `http://127.0.0.1:${server.address().port}` };
}

before(async () => {
  allowedSite = await site();
  deniedSite = await site();
  // Credentials are ephemeral and never printed. Container has no mounts,
  // volumes or shared data; only one randomly assigned loopback API port.
  const created = await run(
    'docker',
    [
      'run',
      '-d',
      '--rm',
      '--name',
      `puxiri-s3-${randomUUID()}`,
      '--label',
      'puxiri.test=s3-spike',
      '--tmpfs',
      '/data:rw,nosuid,nodev,size=128m,mode=1777',
      '-p',
      '127.0.0.1::9000',
      '-e',
      'MINIO_ROOT_USER',
      '-e',
      'MINIO_ROOT_PASSWORD',
      '-e',
      `MINIO_API_CORS_ALLOW_ORIGIN=${allowedSite.origin}`,
      image,
      'server',
      '/data',
      '--address',
      ':9000',
    ],
    {
      env: {
        ...process.env,
        MINIO_ROOT_USER: credentials.accessKeyId,
        MINIO_ROOT_PASSWORD: credentials.secretAccessKey,
      },
      timeout: 60000,
    },
  ).catch((error) => {
    const detail = String(error.stderr || error.code || 'unknown error')
      .replaceAll(credentials.accessKeyId, '[redacted]')
      .replaceAll(credentials.secretAccessKey, '[redacted]')
      .slice(0, 1000);
    throw new Error(`Could not start pinned disposable MinIO: ${detail}`);
  });
  container = created.stdout.trim();
  const published = await run('docker', ['port', container, '9000/tcp']);
  endpoint = `http://${published.stdout.trim()}`;
  for (let attempt = 0; attempt < 100; attempt++) {
    const response = await fetch(`${endpoint}/minio/health/ready`, {
      signal: AbortSignal.timeout(1000),
    }).catch(() => null);
    if (response) {
      await response.arrayBuffer();
      if (response.ok) break;
    }
    assert.notEqual(attempt, 99, 'MinIO readiness timed out');
    await delay(100);
  }
  client = new S3Client({
    endpoint,
    region: 'us-east-1',
    credentials,
    forcePathStyle: true,
    maxAttempts: 1,
    requestHandler: { connectionTimeout: 2000, requestTimeout: 10000 },
    requestChecksumCalculation: 'WHEN_REQUIRED',
  });
  await client.send(new CreateBucketCommand({ Bucket: bucket }), {
    abortSignal: AbortSignal.timeout(10000),
  });
  browser = await chromium.launch();
});

after(async () => {
  await browser?.close();
  client?.destroy();
  for (const origin of [allowedSite, deniedSite]) {
    if (origin) await new Promise((resolve) => origin.server.close(resolve));
  }
  if (container)
    await run('docker', ['rm', '-f', container], { timeout: 10000 });
});

async function http(url, options = {}) {
  try {
    const response = await fetch(url, {
      ...options,
      redirect: 'error',
      signal: AbortSignal.timeout(10000),
    });
    const body = Buffer.from(await response.arrayBuffer());
    return { status: response.status, body };
  } catch {
    // Never bubble a signed URL through fetch error/cause diagnostics.
    throw new Error('S3 spike HTTP transport failed (URL omitted)');
  }
}

async function signedPut(
  key,
  body,
  { conditional = true, expiresIn = 300, signingDate } = {},
) {
  const checksum = createHash('sha256').update(body).digest('base64');
  const headers = {
    'content-type': 'application/octet-stream',
    'x-amz-checksum-sha256': checksum,
  };
  if (conditional) headers['if-none-match'] = '*';
  const url = await getSignedUrl(
    client,
    new PutObjectCommand({
      Bucket: bucket,
      Key: key,
      ContentType: headers['content-type'],
      ChecksumSHA256: checksum,
      ...(conditional ? { IfNoneMatch: '*' } : {}),
    }),
    {
      expiresIn,
      ...(signingDate ? { signingDate } : {}),
      signableHeaders: new Set(['content-type', 'if-none-match']),
      unhoistableHeaders: new Set(['x-amz-checksum-sha256']),
    },
  );
  return { url, headers };
}

async function signedGet(key, expiresIn = 600, signingDate) {
  return getSignedUrl(
    client,
    new GetObjectCommand({ Bucket: bucket, Key: key }),
    {
      expiresIn,
      ...(signingDate ? { signingDate } : {}),
    },
  );
}

test('private bucket rejects anonymous reads/listing; signed access works for four prefixes', async () => {
  const list = await http(`${endpoint}/${bucket}?list-type=2`);
  assert.equal(list.status, 403);
  for (const prefix of prefixes) {
    const key = `${prefix}/${randomUUID()}`;
    const body = Buffer.from(`private ${prefix} fixture`);
    const put = await signedPut(key, body);
    assert.equal(new URL(put.url).searchParams.get('X-Amz-Expires'), '300');
    assert.equal(
      (await http(put.url, { method: 'PUT', body, headers: put.headers }))
        .status,
      200,
    );
    assert.equal((await http(`${endpoint}/${bucket}/${key}`)).status, 403);
    const ttl = ['backups', 'exports'].includes(prefix) ? 3600 : 600;
    const get = await signedGet(key, ttl);
    assert.equal(new URL(get).searchParams.get('X-Amz-Expires'), String(ttl));
    const download = await http(get);
    assert.equal(download.status, 200);
    assert.deepEqual(download.body, body);
  }
});

test('expired or key-tampered URLs are rejected by the real destination', async () => {
  const key = `images/${randomUUID()}`;
  const body = Buffer.from('expiry fixture');
  const put = await signedPut(key, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', body, headers: put.headers })).status,
    200,
  );
  // Backdate the signing instant, not the system clock, to exercise destination expiry.
  const expired = await signedGet(key, 600, new Date(Date.now() - 660000));
  assert.equal((await http(expired)).status, 403);
  const expiredPut = await signedPut(`images/${randomUUID()}`, body, {
    signingDate: new Date(Date.now() - 360000),
  });
  assert.equal(
    (
      await http(expiredPut.url, {
        method: 'PUT',
        body,
        headers: expiredPut.headers,
      })
    ).status,
    403,
  );
  const tampered = new URL(await signedGet(key));
  tampered.pathname = `/${bucket}/avatars/another-key`;
  assert.equal((await http(tampered)).status, 403);
});

test('ordinary signed PUT is reusable and overwrites; signed conditional PUT prevents it', async () => {
  const key = `images/${randomUUID()}`;
  const ordinary = await getSignedUrl(
    client,
    new PutObjectCommand({ Bucket: bucket, Key: key }),
    { expiresIn: 300 },
  );
  assert.equal(
    (await http(ordinary, { method: 'PUT', body: 'first' })).status,
    200,
  );
  assert.equal(
    (await http(ordinary, { method: 'PUT', body: 'replaced' })).status,
    200,
  );
  assert.equal((await http(await signedGet(key))).body.toString(), 'replaced');

  const protectedKey = `images/${randomUUID()}`;
  const body = Buffer.from('immutable fixture');
  const put = await signedPut(protectedKey, body);
  const signedHeaders = new URL(put.url).searchParams
    .get('X-Amz-SignedHeaders')
    .split(';');
  assert.ok(signedHeaders.includes('if-none-match'));
  assert.equal(
    (await http(put.url, { method: 'PUT', body, headers: put.headers })).status,
    200,
  );
  assert.equal(
    (await http(put.url, { method: 'PUT', body, headers: put.headers })).status,
    412,
  );
  const stripped = { ...put.headers };
  delete stripped['if-none-match'];
  // This MinIO release reports a missing signed header as malformed (400),
  // rather than a signature mismatch (403); either must never authorize PUT.
  assert.equal(
    (await http(put.url, { method: 'PUT', body, headers: stripped })).status,
    400,
  );
  assert.deepEqual((await http(await signedGet(protectedKey))).body, body);
});

test('concurrent create-only uploads have one winner and checksum rejects changed bytes', async () => {
  const key = `avatars/${randomUUID()}`;
  const body = Buffer.from('concurrency fixture');
  const put = await signedPut(key, body);
  const changed = await http(put.url, {
    method: 'PUT',
    headers: put.headers,
    body: 'different bytes',
  });
  assert.equal(changed.status, 400);
  const results = await Promise.all(
    [0, 1].map(() =>
      http(put.url, { method: 'PUT', headers: put.headers, body }),
    ),
  );
  assert.deepEqual(results.map((result) => result.status).sort(), [200, 412]);
  assert.deepEqual((await http(await signedGet(key))).body, body);
});

test('HEAD observes real upload size and invalid oversized staging can be removed', async () => {
  const key = `images/${randomUUID()}`;
  // Small fixture limit keeps the test fast; this is not the product's 5 MB limit.
  const reservedBytes = 8;
  const body = Buffer.alloc(16, 1);
  const put = await signedPut(key, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  const head = await client.send(
    new HeadObjectCommand({ Bucket: bucket, Key: key }),
  );
  assert.equal(head.ContentLength, body.length);
  assert.ok(head.ContentLength > reservedBytes);
  await client.send(new DeleteObjectCommand({ Bucket: bucket, Key: key }));
  await assert.rejects(
    client.send(new HeadObjectCommand({ Bucket: bucket, Key: key })),
    (error) => error.$metadata?.httpStatusCode === 404,
  );
  // Deleting the rejected object re-enables the unexpired create-only URL.
  // Thus production needs staging/tombstone/lifecycle protection, not just If-None-Match.
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
});

test('real browser CORS permits the configured origin and blocks a different origin', async () => {
  const key = `images/${randomUUID()}`;
  const body = Buffer.from('browser CORS fixture');
  const put = await signedPut(key, body);
  const get = await signedGet(key);
  const page = await browser.newPage();
  try {
    await page.goto(allowedSite.origin);
    const upload = await page.evaluate(
      async ({ url, headers, body }) => {
        try {
          const response = await fetch(url, {
            method: 'PUT',
            headers,
            body: new Uint8Array(body),
          });
          await response.arrayBuffer();
          return response.status;
        } catch {
          return 'blocked';
        }
      },
      { ...put, body: [...body] },
    );
    assert.equal(upload, 200);
    const downloaded = await page.evaluate(async (url) => {
      try {
        const response = await fetch(url);
        const body = await response.text();
        return { status: response.status, body };
      } catch {
        return { status: 'blocked' };
      }
    }, get);
    assert.deepEqual(downloaded, { status: 200, body: body.toString() });
    await page.goto(deniedSite.origin);
    const blocked = await page.evaluate(async (url) => {
      try {
        const response = await fetch(url);
        await response.arrayBuffer();
        return response.status;
      } catch {
        return 'blocked';
      }
    }, get);
    assert.equal(blocked, 'blocked');
    const deniedKey = `images/${randomUUID()}`;
    const deniedPut = await signedPut(deniedKey, body);
    const deniedUpload = await page.evaluate(
      async ({ url, headers, body }) => {
        try {
          const response = await fetch(url, {
            method: 'PUT',
            headers,
            body: new Uint8Array(body),
          });
          await response.arrayBuffer();
          return response.status;
        } catch {
          return 'blocked';
        }
      },
      { ...deniedPut, body: [...body] },
    );
    assert.equal(deniedUpload, 'blocked');
    await assert.rejects(
      client.send(new HeadObjectCommand({ Bucket: bucket, Key: deniedKey })),
      (error) => error.$metadata?.httpStatusCode === 404,
    );
    // CORS controls browser access, not the bearer capability itself.
    assert.equal((await http(get)).status, 200);
  } finally {
    await page.close();
  }
});

test('server can inventory only the selected prefix for cleanup', async () => {
  const group = randomUUID();
  const body = Buffer.from('prefix inventory fixture');
  const selected = `exports/${group}/`;
  for (const key of [`${selected}file`, `images/${group}/file`]) {
    const put = await signedPut(key, body);
    assert.equal(
      (await http(put.url, { method: 'PUT', headers: put.headers, body }))
        .status,
      200,
    );
  }
  const listed = await client.send(
    new ListObjectsV2Command({ Bucket: bucket, Prefix: selected }),
  );
  assert.equal(listed.Contents.length, 1);
  assert.equal(listed.Contents[0].Key, `${selected}file`);
});

test('staging promotion protects final bytes even when the old upload URL recreates staging', async () => {
  const id = randomUUID();
  const stagingKey = `images/_uploads/${id}`;
  const finalKey = `images/objects/${id}`;
  const body = Buffer.from('validated private image fixture');
  const put = await signedPut(stagingKey, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  const validated = await inspectStaging(client, bucket, stagingKey, {
    maxBytes: body.length,
    sha256: createHash('sha256').update(body).digest('base64'),
  });
  const final = await promoteValidated(client, bucket, validated, finalKey);
  assert.equal(final.bytes, body.length);
  assert.equal(final.sha256, validated.sha256);
  assert.equal((await http(`${endpoint}/${bucket}/${finalKey}`)).status, 403);
  assert.deepEqual((await http(await signedGet(finalKey))).body, body);

  const forged = new URL(put.url);
  forged.pathname = `/${bucket}/${finalKey}`;
  assert.equal(
    (await http(forged, { method: 'PUT', headers: put.headers, body })).status,
    403,
  );
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: stagingKey }),
  );
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  // Physical recreation of temporary bytes is possible before expiry. It must
  // not authorize republishing a terminal upload or overwriting its final key.
  await assert.rejects(
    promoteValidated(client, bucket, validated, finalKey),
    (error) => error.$metadata?.httpStatusCode === 412,
  );
  assert.deepEqual((await http(await signedGet(finalKey))).body, body);
});

test('this MinIO ignores a transmitted destination create-only condition on CopyObject', async () => {
  const source = `images/_uploads/${randomUUID()}`;
  const destination = `images/objects/${randomUUID()}`;
  await client.send(
    new PutObjectCommand({ Bucket: bucket, Key: source, Body: 'replacement' }),
  );
  await client.send(
    new PutObjectCommand({
      Bucket: bucket,
      Key: destination,
      Body: 'existing',
    }),
  );
  const head = await client.send(
    new HeadObjectCommand({ Bucket: bucket, Key: source }),
  );
  const command = new CopyObjectCommand({
    Bucket: bucket,
    Key: destination,
    CopySource: `${bucket}/${source}`,
    CopySourceIfMatch: head.ETag,
    IfNoneMatch: '*',
  });
  command.middlewareStack.add(
    (next) => async (args) => {
      assert.equal(args.request.headers['if-none-match'], '*');
      return next(args);
    },
    { step: 'build', name: 'assertDestinationConditionTransmitted' },
  );
  const result = await client.send(command);
  assert.ok(result.CopyObjectResult.ETag);
  assert.equal(
    (await http(await signedGet(destination))).body.toString(),
    'replacement',
  );
  // This is a compatibility finding, NOT the desired storage contract. The
  // promotion tests use PutObject and fail if destination overwrites are allowed.
});

test('source changed after byte validation is rejected before a final object exists', async () => {
  const id = randomUUID();
  const stagingKey = `avatars/_uploads/${id}`;
  const finalKey = `avatars/objects/${id}`;
  const body = Buffer.from('validated avatar fixture');
  const put = await signedPut(stagingKey, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  const validated = await inspectStaging(client, bucket, stagingKey, {
    maxBytes: body.length,
    sha256: createHash('sha256').update(body).digest('base64'),
  });
  // Deliberately mutate through server credentials to exercise the TOCTOU guard,
  // not through a browser URL (which cannot overwrite the existing staging key).
  await client.send(
    new PutObjectCommand({
      Bucket: bucket,
      Key: stagingKey,
      Body: 'replaced by storage operator',
    }),
  );
  await assert.rejects(
    promoteValidated(client, bucket, validated, finalKey),
    (error) => error.$metadata?.httpStatusCode === 412,
  );
  await assert.rejects(
    client.send(new HeadObjectCommand({ Bucket: bucket, Key: finalKey })),
    (error) => error.$metadata?.httpStatusCode === 404,
  );
});

test('concurrent confirmations cannot overwrite a final key or accept a different existing object', async () => {
  const id = randomUUID();
  const stagingKey = `images/_uploads/${id}`;
  const finalKey = `images/objects/${id}`;
  const body = Buffer.from('concurrent confirmation fixture');
  const put = await signedPut(stagingKey, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  const validated = await inspectStaging(client, bucket, stagingKey, {
    maxBytes: body.length,
    sha256: createHash('sha256').update(body).digest('base64'),
  });
  const outcomes = await Promise.allSettled(
    [0, 1].map(() => promoteValidated(client, bucket, validated, finalKey)),
  );
  assert.equal(
    outcomes.filter((result) => result.status === 'fulfilled').length,
    1,
  );
  const rejected = outcomes.find((result) => result.status === 'rejected');
  assert.equal(rejected.reason.$metadata?.httpStatusCode, 412);
  assert.deepEqual((await http(await signedGet(finalKey))).body, body);
  const collisionKey = `images/objects/${randomUUID()}`;
  await client.send(
    new PutObjectCommand({
      Bucket: bucket,
      Key: collisionKey,
      Body: 'unrelated immutable object',
    }),
  );
  await assert.rejects(
    promoteValidated(client, bucket, validated, collisionKey),
    (error) => error.$metadata?.httpStatusCode === 412,
  );
  assert.equal(
    (await http(await signedGet(collisionKey))).body.toString(),
    'unrelated immutable object',
  );
});

test('invalid staging is not validated; cleanup after expiry cannot recreate it by old URL', async () => {
  const stagingKey = `images/_uploads/${randomUUID()}`;
  const body = Buffer.from('staging bytes fixture');
  const sha256 = createHash('sha256').update(body).digest('base64');
  const put = await signedPut(stagingKey, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  await assert.rejects(
    inspectStaging(client, bucket, stagingKey, {
      maxBytes: body.length - 1,
      sha256,
    }),
    /exceeds the reserved limit/,
  );
  await assert.rejects(
    inspectStaging(client, bucket, stagingKey, {
      maxBytes: body.length,
      sha256: 'incorrect digest',
    }),
    /SHA-256 does not match/,
  );
  const validated = await inspectStaging(client, bucket, stagingKey, {
    maxBytes: body.length,
    sha256,
  });
  await assert.rejects(
    promoteValidated(client, bucket, validated, stagingKey),
    /must differ/,
  );
  // Accelerated real expiry: issue a 1-second capability and wait past its
  // deadline. The fixture waits rather than pretending to revoke the bearer.
  // Separate key: the earlier 300-second capability must not be ignored when
  // deciding cleanup. Production must track the latest expiry of *all* URLs.
  const expiredKey = `images/_uploads/${randomUUID()}`;
  const short = await signedPut(expiredKey, body, { expiresIn: 1 });
  assert.equal(
    (await http(short.url, { method: 'PUT', headers: short.headers, body }))
      .status,
    200,
  );
  const expiresAt = Date.now() + 2100;
  await delay(Math.max(0, expiresAt - Date.now()));
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: expiredKey }),
  );
  assert.equal(
    (await http(short.url, { method: 'PUT', headers: short.headers, body }))
      .status,
    403,
  );
  await assert.rejects(
    client.send(new HeadObjectCommand({ Bucket: bucket, Key: expiredKey })),
    (error) => error.$metadata?.httpStatusCode === 404,
  );
});

test('promotion rejects wrong digest, size overflow and missing source without publishing', async () => {
  const stagingKey = `images/_uploads/${randomUUID()}`;
  const body = Buffer.from('stream validation fixture');
  const put = await signedPut(stagingKey, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  const validated = await inspectStaging(client, bucket, stagingKey, {
    maxBytes: body.length,
    sha256: createHash('sha256').update(body).digest('base64'),
  });
  for (const [candidate, message] of [
    [
      { ...validated, sha256: 'wrong expected digest' },
      /SHA-256 does not match/,
    ],
    [{ ...validated, bytes: body.length - 1 }, /exceed validated size/],
    [{ ...validated, bytes: body.length + 1 }, /byte count/],
  ]) {
    const finalKey = `images/objects/${randomUUID()}`;
    await assert.rejects(
      promoteValidated(client, bucket, candidate, finalKey),
      message,
    );
    await assert.rejects(
      client.send(new HeadObjectCommand({ Bucket: bucket, Key: finalKey })),
      (error) => error.$metadata?.httpStatusCode === 404,
    );
  }
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: stagingKey }),
  );
  const finalKey = `images/objects/${randomUUID()}`;
  await assert.rejects(
    promoteValidated(client, bucket, validated, finalKey),
    (error) => error.$metadata?.httpStatusCode === 404,
  );
  await assert.rejects(
    client.send(new HeadObjectCommand({ Bucket: bucket, Key: finalKey })),
    (error) => error.$metadata?.httpStatusCode === 404,
  );
});

async function lifecycleFixture(t) {
  const root = fileURLToPath(new URL('../../.test-data/', import.meta.url));
  await mkdir(root, { recursive: true });
  const directory = await mkdtemp(join(root, 's3-lifecycle-'));
  const path = join(directory, 'state.sqlite3');
  let store = new LifecycleStore(path);
  t.after(async () => {
    store.close();
    await rm(directory, { recursive: true, force: true });
  });
  const id = randomUUID();
  const body = Buffer.from('durable confirmation fixture');
  const stagingKey = `images/_uploads/${id}`;
  const finalKey = `images/objects/${id}`;
  store.reserve({
    id,
    stagingKey,
    finalKey,
    bytes: body.length,
    sha256: createHash('sha256').update(body).digest('base64'),
  });
  const put = await signedPut(stagingKey, body);
  assert.equal(
    (await http(put.url, { method: 'PUT', headers: put.headers, body })).status,
    200,
  );
  return {
    id,
    body,
    stagingKey,
    finalKey,
    put,
    path,
    get store() {
      return store;
    },
    reopen() {
      store.close();
      store = new LifecycleStore(path);
    },
    otherConnection() {
      return new LifecycleStore(path);
    },
  };
}

test('confirmation recovers remote PUT before SQLite commit and fences the old claim', async (t) => {
  const fixture = await lifecycleFixture(t);
  const first = fixture.store.claim(fixture.id, 100, 10);
  const other = fixture.otherConnection();
  try {
    assert.equal(other.claim(fixture.id, 105, 10), undefined);
  } finally {
    other.close();
  }
  await prepareRemote(client, bucket, first);
  assert.equal(fixture.store.available(fixture.id), undefined);
  assert.equal(fixture.store.objectCount(), 0);
  // Stop/reopen the store at the crash boundary: final object exists but no
  // local availability commit. Recovery must not depend on staging still existing.
  fixture.reopen();
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: fixture.stagingKey }),
  );
  const recovered = fixture.store.claim(fixture.id, 111, 10);
  assert.ok(recovered.epoch > first.epoch);
  const verified = await prepareRemote(client, bucket, recovered);
  assert.equal(fixture.store.complete(first, verified, 112), false);
  assert.equal(fixture.store.complete(recovered, verified, 112), true);
  assert.equal(fixture.store.complete(recovered, verified, 112), false);
  assert.equal(fixture.store.objectCount(), 1);
  assert.equal(fixture.store.available(fixture.id).key, fixture.finalKey);
  assert.deepEqual(
    (await http(await signedGet(fixture.finalKey))).body,
    fixture.body,
  );
});

test('recovery refuses an unrelated existing final object without exposing or overwriting it', async (t) => {
  const fixture = await lifecycleFixture(t);
  const claim = fixture.store.claim(fixture.id, 100, 10);
  await client.send(
    new PutObjectCommand({
      Bucket: bucket,
      Key: fixture.finalKey,
      Body: 'unrelated object',
    }),
  );
  fixture.reopen();
  const recovered = fixture.store.claim(fixture.id, 111, 10);
  await assert.rejects(
    prepareRemote(client, bucket, recovered),
    /byte count or SHA-256 does not match/,
  );
  assert.equal(fixture.store.available(fixture.id), undefined);
  assert.equal(fixture.store.objectCount(), 0);
  assert.equal(fixture.store.complete(claim, {}, 112), false);
  assert.equal(
    (await http(await signedGet(fixture.finalKey))).body.toString(),
    'unrelated object',
  );
});

test('cancelled upload stays terminal after reopen and a late remote write cannot expose it', async (t) => {
  const fixture = await lifecycleFixture(t);
  const claim = fixture.store.claim(fixture.id, 100, 10);
  assert.equal(fixture.store.cancel(fixture.id), true);
  fixture.reopen();
  assert.equal(fixture.store.claim(fixture.id, 111, 10), undefined);
  // A worker already holding credentials may still finish its remote effect.
  // Fencing protects SQLite availability, not an imaginary rollback of S3 PUT.
  const verified = await prepareRemote(client, bucket, claim);
  assert.equal(fixture.store.complete(claim, verified, 112), false);
  assert.equal(fixture.store.available(fixture.id), undefined);
  assert.equal(fixture.store.objectCount(), 0);
  assert.equal(fixture.store.cancel(fixture.id), false);
});

test('durable remote pins block GC and deletion can resume after remote success before SQLite commit', async (t) => {
  const fixture = await lifecycleFixture(t);
  const claim = fixture.store.claim(fixture.id, 100, 10);
  const verified = await prepareRemote(client, bucket, claim);
  assert.equal(fixture.store.complete(claim, verified, 101), true);
  assert.equal(fixture.store.claimGc(fixture.finalKey), undefined); // Still referenced.
  fixture.store.pin('backup-a', fixture.finalKey);
  fixture.store.pin('backup-b', fixture.finalKey);
  fixture.store.unreference(fixture.finalKey);
  fixture.reopen();
  assert.equal(fixture.store.claimGc(fixture.finalKey), undefined);
  assert.deepEqual(
    (await http(await signedGet(fixture.finalKey))).body,
    fixture.body,
  );
  fixture.store.release('backup-a');
  assert.equal(fixture.store.claimGc(fixture.finalKey), undefined);
  fixture.store.release('backup-b');
  assert.equal(fixture.store.claimGc(fixture.finalKey).key, fixture.finalKey);
  assert.equal(fixture.store.pin('too-late', fixture.finalKey), false);
  assert.equal(fixture.store.available(fixture.id), undefined);
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: fixture.finalKey }),
  );
  fixture.reopen();
  assert.equal(fixture.store.deleting()[0].key, fixture.finalKey);
  // Idempotent remote retry after local interruption, then terminal tombstone.
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: fixture.finalKey }),
  );
  assert.equal(fixture.store.deleted(fixture.finalKey), true);
  assert.equal(fixture.store.deleting().length, 0);
  assert.equal(fixture.store.claim(fixture.id, 200, 10), undefined);
  assert.equal(fixture.store.pin('resurrect', fixture.finalKey), false);
  await assert.rejects(
    client.send(
      new HeadObjectCommand({ Bucket: bucket, Key: fixture.finalKey }),
    ),
    (error) => error.$metadata?.httpStatusCode === 404,
  );
});

test('mismatched completion evidence rolls back all state and expired leases cannot commit', async (t) => {
  const fixture = await lifecycleFixture(t);
  const claim = fixture.store.claim(fixture.id, 100, 10);
  const verified = await prepareRemote(client, bucket, claim);
  for (const receipt of [
    { ...verified, key: 'images/objects/wrong-key' },
    { ...verified, bytes: verified.bytes + 1 },
    { ...verified, sha256: 'incorrect digest' },
  ]) {
    assert.throws(
      () => fixture.store.complete(claim, receipt, 101),
      /does not match the reserved object/,
    );
    assert.equal(fixture.store.available(fixture.id), undefined);
    assert.equal(fixture.store.objectCount(), 0);
  }
  assert.equal(fixture.store.complete(claim, verified, 110), false);
  const recovered = fixture.store.claim(fixture.id, 110, 10);
  assert.equal(fixture.store.complete(recovered, verified, 111), true);
  assert.equal(fixture.store.objectCount(), 1);
});

test('overlapping old and renewed workers converge on one verified final key and one registration', async (t) => {
  const fixture = await lifecycleFixture(t);
  const old = fixture.store.claim(fixture.id, 100, 10);
  const renewed = fixture.store.claim(fixture.id, 111, 10);
  const [first, second] = await Promise.all([
    prepareRemote(client, bucket, old),
    prepareRemote(client, bucket, renewed),
  ]);
  assert.equal(first.key, second.key);
  assert.equal(first.sha256, second.sha256);
  assert.equal(fixture.store.complete(old, first, 112), false);
  assert.equal(fixture.store.complete(renewed, second, 112), true);
  assert.equal(fixture.store.objectCount(), 1);
  assert.deepEqual(
    (await http(await signedGet(fixture.finalKey))).body,
    fixture.body,
  );
});

test('actual SIGKILL after remote PUT recovers without a graceful SQLite commit', async (t) => {
  const fixture = await lifecycleFixture(t);
  const worker = fork(
    fileURLToPath(new URL('./s3-interruption-worker.mjs', import.meta.url)),
    [],
    {
      execArgv: [],
      stdio: ['ignore', 'ignore', 'ignore', 'ipc'],
    },
  );
  const exited = once(worker, 'exit');
  let interrupted;
  try {
    worker.send({
      path: fixture.path,
      id: fixture.id,
      config: { endpoint, region: 'us-east-1', credentials, bucket },
    });
    const [message] = await once(worker, 'message', {
      signal: AbortSignal.timeout(15000),
    });
    assert.equal(message.kind, 'remote-ready');
    interrupted = message.claim;
    assert.equal(fixture.store.available(fixture.id), undefined);
    assert.equal(fixture.store.objectCount(), 0);
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
  await client.send(
    new DeleteObjectCommand({ Bucket: bucket, Key: fixture.stagingKey }),
  );
  const recovered = fixture.store.claim(fixture.id, 111, 10);
  const verified = await prepareRemote(client, bucket, recovered);
  assert.equal(fixture.store.complete(interrupted, verified, 112), false);
  assert.equal(fixture.store.complete(recovered, verified, 112), true);
  assert.equal(fixture.store.objectCount(), 1);
  assert.deepEqual(
    (await http(await signedGet(fixture.finalKey))).body,
    fixture.body,
  );
});

registerBundleTests({
  test,
  lifecycleFixture,
  context: () => ({ client, bucket, signedPut, signedGet, http }),
});

registerBundleJobTests({
  test,
  lifecycleFixture,
  context: () => ({ client, bucket, endpoint, credentials }),
});
