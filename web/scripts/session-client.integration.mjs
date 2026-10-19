// Loopback-only Rust fixture runner. Never imported into the browser bundle.
import assert from 'node:assert/strict';
import { URL } from 'node:url';
import {
  getCurrentSession,
  getSessionPage,
  logoutCurrentSession,
  revokeAllSessions,
  revokeSession,
  SessionRequestError,
} from '../src/lib/sessions.ts';

const base = new URL(process.env.PUXIRI_TEST_BASE_URL ?? '');
assert.equal(base.hostname, '127.0.0.1');
assert.equal(base.protocol, 'http:');
const configuredOrigin = `https://${base.host}`;
let cookie = process.env.PUXIRI_TEST_SESSION;
assert.ok(cookie);
const nativeFetch = globalThis.fetch;
globalThis.fetch = async (path, options) => {
  assert.equal(typeof path, 'string');
  assert.ok(path.startsWith('/api/v1/'));
  assert.equal(options.credentials, 'same-origin');
  assert.equal(options.mode, 'same-origin');
  assert.equal(options.redirect, 'error');
  assert.equal(options.body, undefined);
  const headers = new globalThis.Headers(options.headers);
  assert.equal(headers.has('cookie'), false);
  assert.equal(headers.has('origin'), false);
  headers.set('cookie', `__Host-puxiri_session=${cookie}`);
  if (options.method === 'DELETE') {
    headers.set('origin', configuredOrigin);
  }
  return nativeFetch(new URL(path, base), { ...options, headers });
};

const signal = () => new AbortController().signal;
const current = await getCurrentSession(signal());
assert.equal(current.user_id, process.env.PUXIRI_TEST_USER);
assert.equal(current.session_id, process.env.PUXIRI_TEST_CURRENT_ID);
assert.equal(current.csrf_token.length, 43);
let cursor;
const ids = [];
let emptyPages = 0;
for (let pageNumber = 0; ; pageNumber += 1) {
  assert.ok(pageNumber < 4);
  const page = await getSessionPage({ cursor, page_size: 1 }, signal());
  for (const session of page.data) {
    ids.push(session.id);
    assert.equal(session.is_current, session.id === current.session_id);
  }
  if (page.data.length === 0) {
    emptyPages += 1;
  }
  if (!page.has_more) {
    break;
  }
  cursor = page.next_cursor;
}
assert.equal(emptyPages, 1);
assert.deepEqual(
  new Set(ids),
  new Set([current.session_id, process.env.PUXIRI_TEST_TARGET]),
);
await revokeSession(process.env.PUXIRI_TEST_TARGET, signal());
const page = await getSessionPage({}, signal());
assert.equal(page.data.length, 1);
assert.equal(page.data[0].id, current.session_id);
await assert.rejects(
  () => revokeSession(process.env.PUXIRI_TEST_TARGET, signal()),
  (error) =>
    error instanceof SessionRequestError &&
    error.status === 404 &&
    error.code === 'not_found',
);
await revokeAllSessions(signal());
await assert.rejects(
  () => getCurrentSession(signal()),
  (error) => error instanceof SessionRequestError && error.status === 401,
);
cookie = process.env.PUXIRI_TEST_LOGOUT_SESSION;
assert.ok(cookie);
await logoutCurrentSession(signal());
await assert.rejects(
  () => getCurrentSession(signal()),
  (error) => error instanceof SessionRequestError && error.status === 401,
);
