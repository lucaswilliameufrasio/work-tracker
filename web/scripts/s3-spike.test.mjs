// Phase 0: real, disposable MinIO and Chromium. No production storage adapter.
// No operator credentials/endpoints: all configuration is generated locally.
import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createServer } from 'node:http';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { chromium } from '@playwright/test';
import {
  CreateBucketCommand,
  DeleteObjectCommand,
  GetObjectCommand,
  HeadObjectCommand,
  ListObjectsV2Command,
  PutObjectCommand,
  S3Client,
} from '@aws-sdk/client-s3';
import { getSignedUrl } from '@aws-sdk/s3-request-presigner';

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
  ).catch(() => {
    throw new Error(
      'Could not start pinned disposable MinIO; ensure Docker and the pinned image are available',
    );
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
