// Cookie injection is loopback-only fixture code, never part of the browser bundle.
import assert from 'node:assert/strict';
import {
  getIssueDetail,
  getIssueHistoryPage,
  getIssuePage,
  IssueRequestError,
} from '../src/lib/issues.ts';
import {
  getStatusPage,
  getIssueTypePage,
} from '../src/lib/project-catalogs.ts';

const base = new URL(process.env.PUXIRI_TEST_BASE_URL ?? '');
assert.equal(base.hostname, '127.0.0.1');
assert.equal(base.protocol, 'http:');
const project = process.env.PUXIRI_TEST_PROJECT;
assert.ok(project);
let cookie = process.env.PUXIRI_TEST_SESSION;
assert.ok(cookie);
const nativeFetch = globalThis.fetch;
let calls = 0;
globalThis.fetch = async (path, options) => {
  assert.equal(typeof path, 'string');
  assert.ok(
    new RegExp(
      `^/api/v1/projects/${project}/(issues|statuses|issue-types)(/|\\?|$)`,
    ).test(path),
  );
  assert.equal(options.method, 'GET');
  assert.equal(options.credentials, 'same-origin');
  assert.equal(options.mode, 'same-origin');
  assert.equal(options.cache, 'no-store');
  assert.equal(options.redirect, 'error');
  assert.equal(options.body, undefined);
  const headers = new globalThis.Headers(options.headers);
  assert.deepEqual([...headers], [['accept', 'application/json']]);
  headers.set('cookie', `__Host-puxiri_session=${cookie}`);
  calls += 1;
  return nativeFetch(new URL(path, base), { ...options, headers });
};
const signal = () => new AbortController().signal;
const first = await getIssuePage(project, { page_size: 1 }, signal());
assert.equal(first.data[0].issue_number, '1');
assert.equal(first.next_cursor, '1');
const second = await getIssuePage(
  project,
  { page_size: 1, cursor: first.next_cursor },
  signal(),
);
assert.equal(second.data[0].issue_number, '3');
const last = await getIssuePage(
  project,
  { page_size: 1, cursor: second.next_cursor },
  signal(),
);
assert.equal(last.data[0].issue_number, '9007199254740993');
assert.equal(last.data[0].version, '9223372036854775807');
assert.equal(last.has_more, false);
assert.equal('next_cursor' in last, false);
assert.equal('description' in first.data[0], false);
const detail = await getIssueDetail(project, '2', signal());
assert.equal(detail.description, '\u0000'.repeat(65536));
assert.equal(typeof detail.trashed_at, 'string');
assert.equal('history' in detail, false);
const history = await getIssueHistoryPage(
  project,
  '2',
  { page_size: 1 },
  signal(),
);
assert.equal(history.data.length, 1);
assert.equal(history.data[0].issue_id, detail.id);
assert.equal(history.data[0].event_kind, 'issue.created');
const event = history.data[0];
const visibleStatuses = await getStatusPage(
  project,
  { page_size: 2 },
  signal(),
);
assert.equal(visibleStatuses.data.length, 2);
assert.equal(visibleStatuses.has_more, true);
const remainingStatuses = await getStatusPage(
  project,
  { page_size: 2, cursor: visibleStatuses.next_cursor },
  signal(),
);
assert.equal(remainingStatuses.data.length, 2);
assert.equal(remainingStatuses.has_more, false);
const allStatuses = await getStatusPage(
  project,
  { include_archived: true },
  signal(),
);
assert.equal(allStatuses.data.length, 5);
const backlog = allStatuses.data.find(
  (status) => status.system_key === 'backlog',
);
assert.equal(backlog.position, '9223372036854775807');
assert.equal(typeof backlog.archived_at, 'string');
assert.ok(allStatuses.data.some((status) => status.id === detail.status_id));
const activeTypes = await getIssueTypePage(project, {}, signal());
assert.equal(activeTypes.data.length, 0);
const allTypes = await getIssueTypePage(
  project,
  { include_archived: true },
  signal(),
);
assert.equal(allTypes.data.length, 1);
assert.equal(allTypes.data[0].id, detail.issue_type_id);
assert.equal(allTypes.data[0].icon_key, 'opaque_icon');
assert.equal(allTypes.data[0].color_key, 'opaque-color');
const cursor = Buffer.from(`${event.occurred_at}/${event.id}`).toString(
  'base64url',
);
const end = await getIssueHistoryPage(project, '2', { cursor }, signal());
assert.equal(end.data.length, 0);
await assert.rejects(
  () => getIssueDetail(project, '999', signal()),
  (error) => error instanceof IssueRequestError && error.status === 404,
);
cookie = process.env.PUXIRI_TEST_FOREIGN_SESSION;
assert.ok(cookie);
for (const call of [
  () => getIssuePage(project, {}, signal()),
  () => getIssueDetail(project, '1', signal()),
  () => getIssueHistoryPage(project, '1', {}, signal()),
  () => getStatusPage(project, { include_archived: true }, signal()),
  () => getIssueTypePage(project, { include_archived: true }, signal()),
]) {
  await assert.rejects(
    call,
    (error) =>
      error instanceof IssueRequestError &&
      error.status === 404 &&
      error.message === 'Issue request failed',
  );
}
cookie = process.env.PUXIRI_TEST_EXPIRED_SESSION;
assert.ok(cookie);
await assert.rejects(
  () => getIssuePage(project, {}, signal()),
  (error) => error instanceof IssueRequestError && error.status === 401,
);
await assert.rejects(
  () => getStatusPage(project, {}, signal()),
  (error) => error instanceof IssueRequestError && error.status === 401,
);
await assert.rejects(
  () => getIssueTypePage(project, {}, signal()),
  (error) => error instanceof IssueRequestError && error.status === 401,
);
assert.equal(calls, 20);
