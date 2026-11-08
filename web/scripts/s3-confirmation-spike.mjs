// Experimental Phase 0 primitives, deliberately not wired into the product.
// Only server jobs use these credentials; browser transfers remain presigned.
import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream } from 'node:fs';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';
import {
  GetObjectCommand,
  HeadObjectCommand,
  PutObjectCommand,
} from '@aws-sdk/client-s3';

export async function inspectStaging(
  client,
  bucket,
  key,
  { maxBytes, sha256 },
) {
  const head = await client.send(
    new HeadObjectCommand({ Bucket: bucket, Key: key }),
  );
  if (!head.ETag || head.ContentLength > maxBytes) {
    throw new Error(
      'Staging metadata is invalid or exceeds the reserved limit',
    );
  }
  const response = await client.send(
    new GetObjectCommand({
      Bucket: bucket,
      Key: key,
      IfMatch: head.ETag,
    }),
  );
  const digest = createHash('sha256');
  let bytes = 0;
  try {
    for await (const chunk of response.Body) {
      bytes += chunk.length;
      if (bytes > maxBytes)
        throw new Error('Staging bytes exceed the reserved limit');
      digest.update(chunk);
    }
  } finally {
    response.Body.destroy();
  }
  const actual = digest.digest('base64');
  if (bytes !== head.ContentLength || actual !== sha256) {
    throw new Error('Staging byte count or SHA-256 does not match');
  }
  return { key, etag: head.ETag, bytes, sha256: actual };
}

export async function promoteValidated(client, bucket, validated, finalKey) {
  if (validated.key === finalKey)
    throw new Error('Final key must differ from staging');
  // Keep the immutable destination guarantee explicit across providers: spool
  // and verify bounded bytes, then use create-only PutObject rather than COPY.
  const root = fileURLToPath(new URL('../../.test-data/', import.meta.url));
  await mkdir(root, { recursive: true });
  const directory = await mkdtemp(join(root, 's3-confirmation-'));
  try {
    const response = await client.send(
      new GetObjectCommand({
        Bucket: bucket,
        Key: validated.key,
        IfMatch: validated.etag,
      }),
    );
    const digest = createHash('sha256');
    let bytes = 0;
    const bounded = new Transform({
      transform(chunk, _encoding, callback) {
        bytes += chunk.length;
        if (bytes > validated.bytes) {
          callback(new Error('Promotion bytes exceed validated size'));
          return;
        }
        digest.update(chunk);
        callback(null, chunk);
      },
    });
    const path = join(directory, 'validated');
    await pipeline(
      response.Body,
      bounded,
      createWriteStream(path, { flags: 'wx', mode: 0o600 }),
    );
    if (
      bytes !== validated.bytes ||
      digest.digest('base64') !== validated.sha256
    ) {
      throw new Error('Promotion byte count or SHA-256 does not match');
    }
    const body = createReadStream(path);
    try {
      await client.send(
        new PutObjectCommand({
          Bucket: bucket,
          Key: finalKey,
          Body: body,
          ContentLength: bytes,
          ChecksumSHA256: validated.sha256,
          IfNoneMatch: '*',
        }),
      );
    } finally {
      body.destroy();
    }
    // Do not expose a download until actual destination bytes are verified.
    return await inspectStaging(client, bucket, finalKey, {
      maxBytes: validated.bytes,
      sha256: validated.sha256,
    });
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}
