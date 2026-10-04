// Dedicated test child: configuration/key over private IPC only, no error causes.
import { S3Client } from '@aws-sdk/client-s3';
import { LifecycleStore } from './s3-lifecycle-spike.mjs';
import { BundleJobs, runDurableBundle } from './s3-bundle-job-spike.mjs';

process.on('message', async ({ path, root, claim, config, key, point }) => {
  const { bucket, ...clientConfig } = config;
  const client = new S3Client({
    ...clientConfig,
    forcePathStyle: true,
    maxAttempts: 1,
  });
  const store = new LifecycleStore(path);
  const jobs = new BundleJobs(store, root);
  const stop = async () => {
    process.send({ kind: 'boundary', point });
    // IPC keeps the process alive; the parent SIGKILLs without finally handlers.
    await new Promise(() => {});
  };
  try {
    await runDurableBundle({
      jobs,
      claim,
      clock: () => 101,
      sourcePath: path,
      client,
      bucket,
      key: Buffer.from(key),
      keyId: 'job-fixture-key',
      afterPinned: point === 'pins' ? stop : undefined,
      afterSnapshot: point === 'snapshot' ? stop : undefined,
      afterPartial: point === 'partial' ? stop : undefined,
      afterBuilt: point === 'published' ? stop : undefined,
    });
    process.send({ kind: 'unexpected-completion' });
  } catch {
    process.send({ kind: 'failed' });
  } finally {
    client.destroy();
    store.close();
    process.disconnect();
  }
});
