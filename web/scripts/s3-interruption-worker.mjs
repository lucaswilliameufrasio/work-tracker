// Dedicated child for a real SIGKILL at the PUT-before-SQLite-commit boundary.
// Configuration arrives only over the test process's private IPC channel.
import { S3Client } from '@aws-sdk/client-s3';
import { LifecycleStore, prepareRemote } from './s3-lifecycle-spike.mjs';

process.on('message', async ({ path, id, config }) => {
  const { bucket, ...clientConfig } = config;
  const store = new LifecycleStore(path);
  const client = new S3Client({
    ...clientConfig,
    forcePathStyle: true,
    maxAttempts: 1,
    requestHandler: { connectionTimeout: 2000, requestTimeout: 10000 },
    requestChecksumCalculation: 'WHEN_REQUIRED',
  });
  try {
    const claim = store.claim(id, 100, 10);
    await prepareRemote(client, bucket, claim);
    process.send({ kind: 'remote-ready', claim });
    // Keep the IPC channel open. The parent kills us before complete(); no
    // finally/close handlers get an opportunity to simulate a graceful exit.
  } catch {
    process.send({ kind: 'failed' }); // No URLs, credentials, or error causes.
    client.destroy();
    store.close();
    process.exitCode = 1;
    process.disconnect();
  }
});
