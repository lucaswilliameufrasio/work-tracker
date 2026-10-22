import { afterEach, expect, it, vi } from 'vitest';
import { editorRoute, IssueTextScreen, loadEditor } from './issue-text-screen';
import {
  IssueEditIdentityChanged,
  replaceIssueText,
  type IssueTextResult,
} from './issue-text';
import { IssueRequestError } from './issue-transport';
import { SessionRequestError } from './sessions';
import type { IssueDetail } from './issues';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const key = '019a0000-0000-7000-8000-000000000003';
const key2 = '019a0000-0000-7000-8000-000000000004';
const time = '2026-10-22T10:00:00.000000000Z';
const issue: IssueDetail = {
  id: other,
  project_id: id,
  issue_number: '1',
  issue_key: 'DEV-1',
  title: 'Original',
  description: 'Original description',
  issue_type_id: id,
  status_id: id,
  priority: 'none',
  creator_user_id: id,
  version: '1',
  created_at: time,
  updated_at: time,
};
const result: IssueTextResult = {
  receipt: {
    id: other,
    project_id: id,
    issue_number: '1',
    issue_key: 'DEV-1',
    result_version: '2',
    changed: true,
  },
  replayed: false,
};
const draft = { title: 'My draft', description: 'Private draft text' };
const route = { project: id, number: '1' };
const conflict = () =>
  new IssueRequestError(409, 'issue_version_conflict', id, false);
function fixture() {
  const load = vi
    .fn<typeof loadEditor>()
    .mockResolvedValue({ actor: id, issue });
  const send = vi.fn<typeof replaceIssueText>().mockResolvedValue(result);
  const makeKey = vi.fn(() => key);
  const screen = new IssueTextScreen(route, () => undefined, {
    load,
    send,
    key: makeKey,
  });
  return { screen, load, send, makeKey };
}
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it('parses only canonical editor routes without query-carried drafts', () => {
  expect(
    editorRoute(`/projects/${id}/issues/9007199254740993/edit`, ''),
  ).toEqual({ project: id, number: '9007199254740993' });
  for (const path of [
    `/projects/${id}/issues/01/edit`,
    `/projects/${id}/issues/new/edit`,
    `/projects/${id}/issues/1/edit/`,
    '/projects/bad/issues/1/edit',
  ]) {
    expect(editorRoute(path, '')).toBeUndefined();
  }
  expect(
    editorRoute(`/projects/${id}/issues/1/edit`, '?title=private'),
  ).toBeUndefined();
});

it('uses the explicit edit flag, same actor checks and strict detail reads', async () => {
  const session = {
    user_id: id,
    session_id: other,
    csrf_token: 'A'.repeat(43),
  };
  const fetcher = vi.fn<typeof fetch>().mockImplementation(
    async (path) =>
      new Response(
        JSON.stringify(
          path === '/api/v1/auth/session'
            ? session
            : String(path).endsWith('/capabilities')
              ? {
                  project_id: id,
                  can_create_issues: false,
                  can_replace_issue_text: true,
                }
              : issue,
        ),
        { headers: { 'content-type': 'application/json' } },
      ),
  );
  vi.stubGlobal('fetch', fetcher);
  expect(await loadEditor(route, id, new AbortController().signal)).toEqual({
    actor: id,
    issue,
  });
  expect(fetcher).toHaveBeenCalledTimes(4);
  expect(fetcher.mock.calls.map((call) => call[0])).toEqual([
    '/api/v1/auth/session',
    `/api/v1/projects/${id}/capabilities`,
    `/api/v1/projects/${id}/issues/1`,
    '/api/v1/auth/session',
  ]);
});

it('fails closed for older capability responses without the edit flag', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockImplementation(
      async (path) =>
        new Response(
          JSON.stringify(
            path === '/api/v1/auth/session'
              ? { user_id: id, session_id: other, csrf_token: 'A'.repeat(43) }
              : { project_id: id, can_create_issues: true },
          ),
          { headers: { 'content-type': 'application/json' } },
        ),
    );
  vi.stubGlobal('fetch', fetcher);
  const f = fixture();
  const screen = new IssueTextScreen(route, () => undefined, {
    load: loadEditor,
    send: f.send,
    key: f.makeKey,
  });
  await screen.start();
  expect(screen.state.type).toBe('read-only');
  await screen.save();
  expect(f.send).not.toHaveBeenCalled();
  expect(fetcher).toHaveBeenCalledTimes(2);
  screen.dispose();
  f.screen.dispose();
});

it('preserves edited values across concealment and same-actor access revalidation without writes', async () => {
  const f = fixture();
  await f.screen.start();
  f.screen.change(draft);
  f.screen.suspend();
  expect(f.screen.state).toEqual({ type: 'suspended' });
  await f.screen.start();
  expect(f.screen.state).toEqual({ type: 'editing', draft, version: '1' });
  expect(f.load.mock.calls[1]?.[1]).toBe(id);
  expect(f.send).not.toHaveBeenCalled();
  f.screen.dispose();
});

it('preserves conflict drafts and requires read/review/explicit choice before another version/key is saved', async () => {
  const f = fixture();
  f.makeKey.mockReturnValueOnce(key).mockReturnValueOnce(key2);
  f.send.mockRejectedValueOnce(conflict()).mockResolvedValueOnce({
    ...result,
    receipt: { ...result.receipt, result_version: '3' },
  });
  await f.screen.start();
  f.screen.change(draft);
  await f.screen.save();
  expect(f.screen.state).toEqual({ type: 'conflict', draft, version: '1' });
  await f.screen.save();
  await f.screen.start();
  expect(f.load).toHaveBeenCalledTimes(1);
  expect(f.send).toHaveBeenCalledTimes(1);
  const latest = { ...issue, title: 'Concurrent text', version: '2' };
  f.load.mockResolvedValueOnce({ actor: id, issue: latest });
  await f.screen.review();
  expect(f.screen.state).toEqual({
    type: 'review',
    draft,
    version: '1',
    current: latest,
  });
  f.screen.choose('draft');
  expect(f.screen.state).toEqual({ type: 'editing', draft, version: '2' });
  expect(f.send).toHaveBeenCalledTimes(1);
  await f.screen.save();
  expect(f.send.mock.calls[1]?.[3]).toBe(key2);
  expect(f.send.mock.calls[1]?.[4]).toEqual({
    ...draft,
    expected_version: '2',
  });
  expect(f.screen.state.type).toBe('saved');
  f.screen.dispose();
});

it('supports repeated conflicts and explicit discard without sending on review selection', async () => {
  const f = fixture();
  f.send.mockRejectedValue(conflict());
  await f.screen.start();
  f.screen.change(draft);
  await f.screen.save();
  for (const version of ['2', '3']) {
    f.load.mockResolvedValueOnce({
      actor: id,
      issue: { ...issue, version, title: `Current ${version}` },
    });
    await f.screen.review();
    f.screen.choose('draft');
    await f.screen.save();
    expect(f.screen.state).toEqual({ type: 'conflict', draft, version });
  }
  f.load.mockResolvedValueOnce({
    actor: id,
    issue: { ...issue, version: '4', title: 'Reviewed current' },
  });
  await f.screen.review();
  f.screen.choose('current');
  expect(f.screen.state).toEqual({
    type: 'editing',
    draft: { title: 'Reviewed current', description: issue.description },
    version: '4',
  });
  expect(f.send).toHaveBeenCalledTimes(3);
  f.screen.dispose();
});

it('locks uncertain commands to the original key/version and never infers success from a later read', async () => {
  const f = fixture();
  f.send
    .mockRejectedValueOnce(new Error('network'))
    .mockResolvedValueOnce({ ...result, replayed: true });
  await f.screen.start();
  f.screen.change(draft);
  await f.screen.save();
  expect(f.screen.state.type).toBe('uncertain');
  f.screen.change({ title: 'Not allowed', description: '' });
  await f.screen.save();
  f.screen.suspend();
  f.load.mockResolvedValueOnce({
    actor: id,
    issue: { ...issue, ...draft, version: '2' },
  });
  await f.screen.start();
  expect(f.screen.state.type).toBe('uncertain');
  expect(f.send).toHaveBeenCalledTimes(1);
  await f.screen.retry();
  expect(f.send.mock.calls[1]?.slice(0, 5)).toEqual(
    f.send.mock.calls[0]?.slice(0, 5),
  );
  expect(f.makeKey).toHaveBeenCalledTimes(1);
  expect(f.screen.state.type).toBe('saved');
  f.screen.dispose();
});

it('purges drafts/commands on identity change, revoked access or session expiry but not network read failures', async () => {
  for (const error of [
    new IssueEditIdentityChanged(),
    new SessionRequestError(401, 'unauthenticated', id, false),
    new IssueRequestError(404, 'not_found', id, false),
  ]) {
    const f = fixture();
    await f.screen.start();
    f.screen.change(draft);
    f.screen.suspend();
    f.load.mockRejectedValueOnce(error);
    await f.screen.start();
    expect(['unavailable', 'signed-out']).toContain(f.screen.state.type);
    await f.screen.start();
    expect(f.screen.state).toEqual({
      type: 'editing',
      draft: { title: issue.title, description: issue.description },
      version: '1',
    });
    expect(f.send).not.toHaveBeenCalled();
    f.screen.dispose();
  }
  const f = fixture();
  await f.screen.start();
  f.screen.change(draft);
  f.screen.suspend();
  f.load.mockRejectedValueOnce(new Error('offline'));
  await f.screen.start();
  expect(f.screen.state.type).toBe('read-error');
  await f.screen.start();
  expect(f.screen.state).toEqual({ type: 'editing', draft, version: '1' });
  f.screen.dispose();
});

it('ignores late save/read completions after suspension and disposal', async () => {
  const f = fixture();
  let finish: ((value: IssueTextResult) => void) | undefined;
  f.send.mockImplementation(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  await f.screen.start();
  f.screen.change(draft);
  const saving = f.screen.save();
  f.screen.suspend();
  await f.screen.start();
  expect(f.screen.state.type).toBe('uncertain');
  finish?.(result);
  await saving;
  expect(f.screen.state.type).toBe('uncertain');
  f.screen.dispose();
  const g = fixture();
  let read:
    ((value: { actor: string; issue: IssueDetail }) => void) | undefined;
  g.load.mockImplementation(
    () =>
      new Promise((resolve) => {
        read = resolve;
      }),
  );
  const starting = g.screen.start();
  g.screen.dispose();
  read?.({ actor: id, issue });
  await starting;
  expect(g.screen.state.type).toBe('suspended');
});

it('detects a changed server version on return and keeps draft rather than silently rebasing', async () => {
  const f = fixture();
  await f.screen.start();
  f.screen.change(draft);
  f.screen.suspend();
  f.load.mockResolvedValueOnce({
    actor: id,
    issue: { ...issue, version: '2' },
  });
  await f.screen.start();
  expect(f.screen.state).toEqual({ type: 'conflict', draft, version: '1' });
  expect(f.makeKey).not.toHaveBeenCalled();
  expect(f.send).not.toHaveBeenCalled();
  f.screen.dispose();
});
