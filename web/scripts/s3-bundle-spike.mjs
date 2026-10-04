// Phase 0 only: experimental bounded TAR + AES-GCM envelope, not a public
// backup format, key-management service, retention engine, or product restore.
import { backup, DatabaseSync } from 'node:sqlite';
import {
  createCipheriv,
  createDecipheriv,
  createHash,
  randomBytes,
} from 'node:crypto';
import { createReadStream, createWriteStream } from 'node:fs';
import {
  appendFile,
  chmod,
  link,
  mkdir,
  mkdtemp,
  open,
  readFile,
  rm,
  stat,
  unlink,
  writeFile,
} from 'node:fs/promises';
import { join } from 'node:path';
import { Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { c as pack, x as unpack } from 'tar';
import { GetObjectCommand } from '@aws-sdk/client-s3';

const LIMIT = 8 * 1024 * 1024; // Fixture safety budget, not a product backup limit.
const format = 'puxiri-bundle-spike-v0';

function bounded(limit, digest = null) {
  let bytes = 0;
  return new Transform({
    transform(chunk, _encoding, callback) {
      bytes += chunk.length;
      if (bytes > limit)
        return callback(new Error('Bundle fixture exceeds safety budget'));
      digest?.update(chunk);
      callback(null, chunk);
    },
  });
}

async function checksum(path) {
  const digest = createHash('sha256');
  for await (const chunk of createReadStream(path)) digest.update(chunk);
  return { bytes: (await stat(path)).size, sha256: digest.digest('base64') };
}

export async function freezeSnapshot(
  store,
  sourcePath,
  job,
  directory,
  afterPinned = async () => {},
  guardFreeze = () => {},
) {
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const reader = new DatabaseSync(sourcePath, { readOnly: true });
  let inventory;
  store.db.exec('BEGIN IMMEDIATE');
  try {
    guardFreeze();
    reader.exec('BEGIN');
    inventory = reader
      .prepare(
        `SELECT DISTINCT objects.key, objects.sha256, objects.bytes
      FROM objects JOIN refs ON refs.object_key = objects.key WHERE objects.state = 'available'
      AND (objects.key LIKE 'images/objects/%' OR objects.key LIKE 'avatars/objects/%')
      ORDER BY objects.key`,
      )
      .all();
    for (const object of inventory) {
      store.db
        .prepare('INSERT INTO pins (job, object_key) VALUES (?, ?)')
        .run(job, object.key);
    }
    store.db.exec('COMMIT');
  } catch (error) {
    store.db.exec('ROLLBACK');
    reader.close();
    throw error;
  }
  try {
    await afterPinned(inventory);
    const path = join(directory, 'database.sqlite3');
    await backup(reader, path, { rate: 1 });
    await chmod(path, 0o600);
    reader.exec('COMMIT');
    return { path, inventory };
  } finally {
    reader.close();
  }
}

export async function sealArchive(
  stream,
  output,
  key,
  keyId,
  afterPartial = async () => {},
) {
  if (key.length !== 32) throw new Error('AES-256 key must contain 32 bytes');
  const iv = randomBytes(12);
  const header = Buffer.from(
    JSON.stringify({ format, keyId, iv: iv.toString('base64') }),
  );
  if (header.length > 1024)
    throw new Error('Envelope header exceeds fixture limit');
  const length = Buffer.alloc(4);
  length.writeUInt32BE(header.length);
  const aad = Buffer.concat([length, header]);
  const cipher = createCipheriv('aes-256-gcm', key, iv);
  cipher.setAAD(aad);
  const partial = `${output}.partial`;
  await writeFile(partial, aad, { flag: 'wx', mode: 0o600 });
  try {
    await afterPartial();
    await pipeline(
      stream,
      bounded(LIMIT),
      cipher,
      createWriteStream(partial, { flags: 'a' }),
    );
    await appendFile(partial, cipher.getAuthTag());
    const handle = await open(partial, 'r+');
    try {
      await handle.sync();
    } finally {
      await handle.close();
    }
    // Exclusive atomic publication: never replace an existing artifact.
    await link(partial, output);
  } finally {
    await unlink(partial);
  }
}

export async function buildBundle({
  store,
  sourcePath,
  client,
  bucket,
  job,
  directory,
  output,
  key,
  keyId,
  afterPinned,
  guardFreeze,
  afterSnapshot = async () => {},
  afterPartial,
  releasePins = true,
}) {
  try {
    const snapshot = await freezeSnapshot(
      store,
      sourcePath,
      job,
      directory,
      afterPinned,
      guardFreeze,
    );
    await afterSnapshot(snapshot);
    const reservedBytes = snapshot.inventory.reduce(
      (total, object) => total + object.bytes,
      0,
    );
    if (reservedBytes + (await stat(snapshot.path)).size > LIMIT) {
      throw new Error('Bundle fixture exceeds safety budget');
    }
    const files = [];
    const missing = [];
    await mkdir(join(directory, 'objects'), { mode: 0o700 });
    for (const [index, object] of snapshot.inventory.entries()) {
      const path = `objects/${index}.bin`;
      const digest = createHash('sha256');
      try {
        const response = await client.send(
          new GetObjectCommand({ Bucket: bucket, Key: object.key }),
        );
        await pipeline(
          response.Body,
          bounded(object.bytes, digest),
          createWriteStream(join(directory, path), {
            flags: 'wx',
            mode: 0o600,
          }),
        );
        const actual = {
          bytes: (await stat(join(directory, path))).size,
          sha256: digest.digest('base64'),
        };
        if (actual.bytes !== object.bytes || actual.sha256 !== object.sha256) {
          throw new Error('Remote bytes differ from snapshot inventory');
        }
        files.push({ ...object, path });
      } catch (error) {
        await rm(join(directory, path), { force: true });
        missing.push({
          key: object.key,
          reason:
            error.$metadata?.httpStatusCode === 404 ? 'missing' : 'unverified',
        });
      }
    }
    const database = await checksum(snapshot.path);
    const manifest = { format, keyId, database, files };
    await writeFile(
      join(directory, 'manifest.json'),
      JSON.stringify(manifest),
      { flag: 'wx', mode: 0o600 },
    );
    if (missing.length) {
      // Preserve database/inventory for diagnosis; never publish or run retention.
      await writeFile(
        join(directory, 'incomplete.json'),
        JSON.stringify(missing),
        { flag: 'wx', mode: 0o600 },
      );
      return { status: 'incomplete', missing, directory };
    }
    await sealArchive(
      pack({ cwd: directory, portable: true, strict: true }, [
        'database.sqlite3',
        'manifest.json',
        ...files.map((file) => file.path),
      ]),
      output,
      key,
      keyId,
      afterPartial,
    );
    return {
      status: 'complete',
      manifest,
      output,
      bytes: (await stat(output)).size,
    };
  } finally {
    if (releasePins) store.release(job);
  }
}

async function decryptArchive(input, output, key, expectedKeyId) {
  const handle = await open(input, 'r');
  try {
    const size = (await handle.stat()).size;
    if (size > LIMIT + 2048 || size < 21)
      throw new Error('Invalid envelope size');
    const length = Buffer.alloc(4);
    await handle.read(length, 0, 4, 0);
    const n = length.readUInt32BE();
    if (n > 1024 || n < 1 || size < 4 + n + 16)
      throw new Error('Invalid envelope header length');
    const header = Buffer.alloc(n);
    await handle.read(header, 0, n, 4);
    const envelope = JSON.parse(header.toString());
    if (envelope.format !== format || envelope.keyId !== expectedKeyId)
      throw new Error('Unsupported envelope or key id');
    const iv = Buffer.from(envelope.iv, 'base64');
    if (iv.length !== 12) throw new Error('Invalid envelope nonce');
    const tag = Buffer.alloc(16);
    await handle.read(tag, 0, 16, size - 16);
    const decipher = createDecipheriv('aes-256-gcm', key, iv);
    decipher.setAAD(Buffer.concat([length, header]));
    decipher.setAuthTag(tag);
    // No TAR entries are extracted until GCM authentication has completed.
    await pipeline(
      handle.createReadStream({
        start: 4 + n,
        end: size - 17,
        autoClose: false,
      }),
      decipher,
      bounded(LIMIT),
      createWriteStream(output, { flags: 'wx', mode: 0o600 }),
    );
  } finally {
    await handle.close();
  }
}

export async function restoreIsolated(input, key, keyId, parent) {
  const directory = await mkdtemp(join(parent, 'restore-'));
  try {
    const archive = join(directory, 'authenticated.tar');
    await decryptArchive(input, archive, key, keyId);
    const extracted = join(directory, 'extracted');
    await mkdir(extracted, { mode: 0o700 });
    const entries = new Set();
    let invalid = false;
    let extractedBytes = 0;
    await pipeline(
      createReadStream(archive),
      unpack({
        cwd: extracted,
        strict: true,
        preservePaths: false,
        filter(path, entry) {
          extractedBytes += entry.size;
          if (
            !['File', 'Directory'].includes(entry.type) ||
            entries.has(path) ||
            !Number.isSafeInteger(entry.size) ||
            entry.size < 0 ||
            extractedBytes > LIMIT ||
            !/^(database\.sqlite3|manifest\.json|objects\/|objects\/\d+\.bin)$/.test(
              path,
            )
          ) {
            invalid = true;
            return false;
          }
          entries.add(path);
          return true;
        },
      }),
    );
    if (invalid)
      throw new Error('Archive contains forbidden or duplicate entries');
    const manifestPath = join(extracted, 'manifest.json');
    if ((await stat(manifestPath)).size > 65536)
      throw new Error('Manifest exceeds fixture limit');
    const manifest = JSON.parse(await readFile(manifestPath, 'utf8'));
    if (
      manifest.format !== format ||
      manifest.keyId !== keyId ||
      !Array.isArray(manifest.files)
    ) {
      throw new Error('Unsupported manifest');
    }
    const expected = new Set(['database.sqlite3', 'manifest.json']);
    const keys = new Set();
    for (const object of manifest.files) {
      if (
        !/^objects\/\d+\.bin$/.test(object.path) ||
        expected.has(object.path) ||
        keys.has(object.key)
      ) {
        throw new Error('Invalid manifest paths or duplicate objects');
      }
      expected.add(object.path);
      keys.add(object.key);
    }
    const actualFiles = [...entries].filter((entry) => entry !== 'objects/');
    if (
      actualFiles.length !== expected.size ||
      actualFiles.some((entry) => !expected.has(entry))
    ) {
      throw new Error('Archive does not match the complete inventory');
    }
    for (const object of [
      { ...manifest.database, path: 'database.sqlite3' },
      ...manifest.files,
    ]) {
      const actual = await checksum(join(extracted, object.path));
      if (actual.bytes !== object.bytes || actual.sha256 !== object.sha256)
        throw new Error('Restore checksum mismatch');
    }
    const databasePath = join(extracted, 'database.sqlite3');
    const db = new DatabaseSync(databasePath, { readOnly: true });
    try {
      if (db.prepare('PRAGMA integrity_check').get().integrity_check !== 'ok')
        throw new Error('Restored SQLite failed integrity check');
      if (db.prepare('PRAGMA foreign_key_check').all().length)
        throw new Error('Restored SQLite failed foreign key check');
      const refs = db
        .prepare(
          `SELECT DISTINCT objects.key, objects.bytes, objects.sha256 FROM objects
        JOIN refs ON refs.object_key = objects.key WHERE objects.state = 'available'
        AND (objects.key LIKE 'images/objects/%' OR objects.key LIKE 'avatars/objects/%') ORDER BY objects.key`,
        )
        .all();
      if (
        refs.length !== manifest.files.length ||
        refs.some(
          (row) =>
            !manifest.files.some(
              (file) =>
                row.key === file.key &&
                row.bytes === file.bytes &&
                row.sha256 === file.sha256,
            ),
        )
      ) {
        throw new Error(
          'Restored database references do not match bundle files',
        );
      }
    } finally {
      db.close();
    }
    await rm(archive);
    return { directory, databasePath, manifest, extracted };
  } catch (error) {
    await rm(directory, { recursive: true, force: true });
    throw error;
  }
}
