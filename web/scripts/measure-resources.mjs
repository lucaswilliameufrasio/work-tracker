// Optional Phase 0 measurements. Not part of the product or a capacity promise.
import { execFile } from 'node:child_process';
import assert from 'node:assert/strict';
import { promisify } from 'node:util';
import { randomUUID } from 'node:crypto';
import { mkdir, mkdtemp, rm, statfs, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const run = promisify(execFile);
const root = fileURLToPath(new URL('../../', import.meta.url));
const testData = join(root, '.test-data');
const image =
  'rust@sha256:f49565f188ee00bc2a18dd418183f2c5f23ef7d6e691890517ed341a598f67c3';
await mkdir(testData, { recursive: true });
const storage = await statfs(testData);
const { stdout: security } = await run('docker', [
  'info',
  '--format',
  '{{json .SecurityOptions}}',
]);
// Rootless Docker maps container uid 0 to the invoking unprivileged host user.
// Other uids do not own this 0700 bind mount; never solve that by chmod 0777.
const rootless = JSON.parse(security).includes('name=rootless');
const user = rootless ? '0:0' : `${process.getuid()}:${process.getgid()}`;
const reports = [];
for (const [cpus, memory] of [
  [1, '1g'],
  [2, '2g'],
]) {
  for (let repetition = 1; repetition <= 2; repetition++) {
    const directory = await mkdtemp(join(testData, 'resource-'));
    const name = `puxiri-resource-${randomUUID()}`;
    try {
      const result = await run(
        'docker',
        [
          'run',
          '--rm',
          '--name',
          name,
          '--label',
          'puxiri.test=resource-spike',
          '--network',
          'none',
          '--read-only',
          '--cap-drop',
          'ALL',
          '--security-opt',
          'no-new-privileges',
          '--pids-limit',
          '32',
          '--cpus',
          String(cpus),
          '--memory',
          memory,
          '--memory-swap',
          memory,
          '--user',
          user,
          '--env',
          'TMPDIR=/data',
          '--mount',
          `type=bind,src=${directory},dst=/data`,
          '--mount',
          `type=bind,src=${join(root, 'target/release/examples/resource_spike')},dst=/probe,readonly`,
          '--entrypoint',
          '/probe',
          image,
        ],
        { timeout: 60000, maxBuffer: 1024 * 1024 },
      );
      const report = JSON.parse(result.stdout);
      assert.equal(
        report.cgroup_cpu_max.trim(),
        `${cpus * 100000} 100000`,
        'CPU quota was not applied',
      );
      assert.equal(
        Number(report.cgroup_memory_max.trim()),
        cpus * 1024 ** 3,
        'Memory limit was not applied',
      );
      reports.push({ cpus, memory, repetition, report });
    } finally {
      // Exact exclusive container/directory only; never touch other runs.
      await run('docker', ['rm', '-f', name], { timeout: 10000 }).catch(
        (error) => {
          if (!error.stderr?.includes('No such container')) throw error;
        },
      );
      await rm(directory, { recursive: true, force: true });
    }
  }
}
const report = {
  image,
  rootless,
  containerUser: user,
  storage: {
    kind: 'private host bind volume, NOT tmpfs',
    filesystemType: storage.type,
    note: 'SSD/IO isolation and real product workload not certified',
  },
  reports,
};
await mkdir(join(testData, 'reports'), { recursive: true });
const output = join(testData, 'reports', `snapshot-${randomUUID()}.json`);
await writeFile(output, JSON.stringify(report, null, 2), {
  flag: 'wx',
  mode: 0o600,
});
console.log(
  JSON.stringify(
    {
      output,
      image,
      rootless,
      storage: report.storage,
      measurements: reports.map(({ cpus, memory, repetition, report }) => ({
        cpus,
        memory,
        repetition,
        freezeMs: report.snapshot.freeze_writer_lock_ms,
        readerHoldMs: report.snapshot.reader_hold_ms,
        writeP95Ms: report.write_transaction_ms.p95,
        walHeldBytes: report.wal_held_bytes,
        walAfterReleaseBytes: report.wal_after_reader_release_bytes,
        dataset: {
          issues: report.issues,
          fieldValues: report.field_values,
          comments: report.comments,
          events: report.events,
          indexedDocuments: report.fts_documents,
        },
        search: {
          queryCount: report.search.query_count,
          returnedRows: report.search.returned_rows,
          unauthorizedRows: report.search.unauthorized_rows,
          maxRowsPerQuery: report.search.max_rows_per_query,
          queryP95Ms: report.search.query_p95_ms,
          queryTotalMs: report.search.query_total_ms,
          queryPlan: report.search.query_plan,
        },
        processMemory: report.proc_status,
        cgroupMemoryPeakBytes: Number(report.cgroup_memory_peak),
        argon2id: {
          sequential: report.argon2id.sequential.map(
            ({ memory_kib, iterations, median_ms, p95_ms }) => ({
              memory_kib,
              iterations,
              median_ms,
              p95_ms,
            }),
          ),
          concurrencyBurst: report.argon2id.concurrency_burst,
          processMemoryAfterKdf: report.argon2id.process_status_after_kdf,
          wholeRunCgroupPeakAfterKdfBytes: Number(
            report.argon2id.cgroup_memory_peak_after_kdf,
          ),
        },
      })),
    },
    null,
    2,
  ),
);
