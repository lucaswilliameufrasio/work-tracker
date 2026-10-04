import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import { resolve } from 'node:path';
import { spawn } from 'node:child_process';

const root = resolve('..');
const temporaryRoot = resolve(root, '.test-data');
await mkdir(temporaryRoot, { recursive: true });
const directory = await mkdtemp(resolve(temporaryRoot, 'e2e-'));
const child = spawn(
  resolve(root, 'target/debug/puxiri'),
  ['serve', '--bind', '127.0.0.1:4174', '--data-dir', directory],
  { cwd: directory, stdio: 'inherit' },
);

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => child.kill(signal));
}
child.on('error', async (error) => {
  console.error(error.message);
  await rm(directory, { recursive: true, force: true });
  process.exit(1);
});
child.on('exit', async (code, signal) => {
  await rm(directory, { recursive: true, force: true });
  process.exit(signal ? 1 : (code ?? 1));
});
