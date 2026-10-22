// Loopback-only fixture injection, never shipped in the browser bundle.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { createIssue } from '../src/lib/issue-creation.ts';
import { IssueRequestError } from '../src/lib/issue-transport.ts';
import { getIssueDetail } from '../src/lib/issues.ts';

const base = new URL(process.env.PUXIRI_TEST_BASE_URL ?? '');
assert.equal(base.hostname, '127.0.0.1');
assert.equal(base.protocol, 'http:');
const project = process.env.PUXIRI_TEST_PROJECT;
const actor = process.env.PUXIRI_TEST_ACTOR;
assert.ok(project && actor);
let cookie = process.env.PUXIRI_TEST_SESSION;
assert.ok(cookie);
let loseResponse = false;
let posts = 0;
const nativeFetch = globalThis.fetch;
globalThis.fetch = async (path, options) => {
  assert.equal(typeof path, 'string');
  assert.ok(
    path === '/api/v1/auth/session' ||
      path.startsWith(`/api/v1/projects/${project}/issues`),
  );
  assert.equal(options.credentials, 'same-origin');
  assert.equal(options.mode, 'same-origin');
  assert.equal(options.cache, 'no-store');
  assert.equal(options.redirect, 'error');
  const headers = new globalThis.Headers(options.headers);
  assert.equal(headers.has('authorization'), false);
  assert.equal(headers.has('origin'), false);
  headers.set('cookie', `__Host-puxiri_session=${cookie}`);
  if (options.method === 'POST') {
    assert.equal(path, `/api/v1/projects/${project}/issues`);
    headers.set('origin', `https://${base.host}`);
    posts += 1;
  } else {
    assert.equal(options.method, 'GET');
  }
  const response = await nativeFetch(new URL(path, base), {
    ...options,
    headers,
  });
  if (options.method === 'POST' && loseResponse) {
    loseResponse = false;
    assert.equal(response.status, 201);
    await response.arrayBuffer();
    throw new Error('Synthetic response loss after commit');
  }
  return response;
};
const signal = () => new AbortController().signal;
const key = randomUUID();
const input = {
  title: ' Raw <script>title</script> ',
  description: 'é'.repeat(32768),
};
const first = await createIssue(project, actor, key, input, signal());
assert.equal(first.replayed, false);
assert.equal(first.receipt.issue_number, '9007199254740993');
assert.equal(first.receipt.created_version, '1');
assert.equal(Object.keys(first.receipt).length, 5);
const detail = await getIssueDetail(
  project,
  first.receipt.issue_number,
  signal(),
);
assert.equal(detail.title, input.title.trim());
assert.equal(detail.description, input.description);
const replay = await createIssue(project, actor, key, input, signal());
assert.equal(replay.replayed, true);
assert.deepEqual(replay.receipt, first.receipt);
await assert.rejects(
  createIssue(project, actor, key, { title: 'Changed' }, signal()),
  (error) =>
    error instanceof IssueRequestError &&
    error.status === 409 &&
    error.code === 'idempotency_conflict',
);
const lostKey = randomUUID();
const lostInput = { title: 'Uncertain committed response' };
loseResponse = true;
const before = posts;
await assert.rejects(
  createIssue(project, actor, lostKey, lostInput, signal()),
  /result could not be confirmed/,
);
assert.equal(posts, before + 1);
const confirmed = await createIssue(
  project,
  actor,
  lostKey,
  lostInput,
  signal(),
);
assert.equal(confirmed.replayed, true);
assert.equal(confirmed.receipt.issue_number, '9007199254740994');
cookie = process.env.PUXIRI_TEST_FOREIGN_SESSION;
const foreignActor = process.env.PUXIRI_TEST_FOREIGN_ACTOR;
assert.ok(cookie && foreignActor);
const privateBefore = posts;
await assert.rejects(
  createIssue(project, actor, key, input, signal()),
  /identity changed/,
);
assert.equal(posts, privateBefore);
await assert.rejects(
  createIssue(project, foreignActor, key, input, signal()),
  (error) => error instanceof IssueRequestError && error.status === 404,
);
