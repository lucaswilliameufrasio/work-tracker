import { afterEach, expect, it, vi } from 'vitest';
import {
  creationAccess,
  creationProject,
  IssueCreationScreen,
  type CreationState,
} from './issue-creation-screen';
import { createIssue, type IssueCreationResult } from './issue-creation';
import { parseProjectCapabilities } from './projects';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const key = '019a0000-0000-7000-8000-000000000003';
const result: IssueCreationResult = {
  receipt: {
    id: other,
    project_id: id,
    issue_key: 'DEV-1',
    issue_number: '1',
    created_version: '1',
  },
  replayed: false,
};
afterEach(() => vi.unstubAllGlobals());

function fixture() {
  const access = vi.fn<typeof creationAccess>().mockResolvedValue(id);
  const send = vi.fn<typeof createIssue>().mockResolvedValue(result);
  const makeKey = vi.fn(() => key);
  const views: CreationState[] = [];
  const screen = new IssueCreationScreen(id, (state) => views.push(state), {
    access,
    send,
    key: makeKey,
  });
  return { screen, access, send, makeKey, views };
}

it('recognizes only canonical dedicated creation URLs without query payloads', () => {
  expect(creationProject(`/projects/${id}/issues/new`, '')).toBe(id);
  for (const [path, query] of [
    [`/projects/${id}/issues/new`, '?title=private'],
    ['/projects/bad/issues/new', ''],
    [`/projects/${id}/issues/new/`, ''],
    [`/projects/${id}/issues/1`, ''],
  ]) {
    expect(creationProject(path ?? '', query ?? '')).toBeUndefined();
  }
});

it('checks session identity and advisory mutation capability using fixed GET', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          user_id: id,
          session_id: other,
          csrf_token: 'A'.repeat(43),
        }),
        { headers: { 'content-type': 'application/json' } },
      ),
    )
    .mockResolvedValueOnce(
      new Response(
        JSON.stringify({ project_id: id, can_create_issues: true }),
        { headers: { 'content-type': 'application/json' } },
      ),
    );
  vi.stubGlobal('fetch', fetcher);
  expect(await creationAccess(id, id, new AbortController().signal)).toBe(id);
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${id}/capabilities`,
  );
  expect(fetcher.mock.calls[1]?.[1]).toMatchObject({
    method: 'GET',
    mode: 'same-origin',
    credentials: 'same-origin',
    cache: 'no-store',
    redirect: 'error',
  });
  expect(fetcher.mock.calls[1]?.[1]?.body).toBeUndefined();
});

it('rejects corrupt capability DTOs and hides form/retry for known read-only access', async () => {
  for (const value of [
    null,
    {},
    { project_id: other, can_create_issues: true },
    { project_id: id, can_create_issues: 'true' },
    { project_id: id, can_create_issues: null },
    { project_id: id, can_create_issues: true, token: 'untrusted' },
  ]) {
    expect(() => parseProjectCapabilities(value, id)).toThrow();
  }
  expect(
    parseProjectCapabilities({ project_id: id, can_create_issues: false }, id)
      .can_create_issues,
  ).toBe(false);
  const send = vi.fn<typeof createIssue>();
  const fetcher = vi
    .fn<typeof fetch>()
    .mockImplementation(
      async (path) =>
        new Response(
          JSON.stringify(
            path === '/api/v1/auth/session'
              ? { user_id: id, session_id: other, csrf_token: 'A'.repeat(43) }
              : { project_id: id, can_create_issues: false },
          ),
          { headers: { 'content-type': 'application/json' } },
        ),
    );
  vi.stubGlobal('fetch', fetcher);
  const screen = new IssueCreationScreen(id, () => undefined, {
    access: creationAccess,
    send,
    key: () => key,
  });
  await screen.start();
  expect(screen.state.type).toBe('read-only');
  await screen.submit({ title: 'Not permitted' });
  await screen.retry();
  expect(send).not.toHaveBeenCalled();
  screen.dispose();
});

it('snapshots one submission and blocks duplicate submits and retry after confirmation', async () => {
  const f = fixture();
  let finish: ((value: IssueCreationResult) => void) | undefined;
  f.send.mockImplementation(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  await f.screen.start();
  expect(f.screen.state.type).toBe('editing');
  const input = { title: 'Raw', description: 'Original' };
  const sending = f.screen.submit(input);
  input.title = 'Changed';
  expect(f.screen.state.type).toBe('submitting');
  await f.screen.submit({ title: 'Duplicate' });
  await f.screen.retry();
  expect(f.send).toHaveBeenCalledTimes(1);
  expect(f.send.mock.calls[0]?.slice(0, 4)).toEqual([
    id,
    id,
    key,
    { title: 'Raw', description: 'Original' },
  ]);
  finish?.(result);
  await sending;
  expect(f.screen.state).toEqual({
    type: 'confirmed',
    receipt: result.receipt,
  });
  await f.screen.retry();
  await f.screen.submit({ title: 'Not a new operation' });
  expect(f.makeKey).toHaveBeenCalledTimes(1);
  expect(f.send).toHaveBeenCalledTimes(1);
  f.screen.dispose();
});

it('retries only by explicit action with exactly the same actor, payload and key', async () => {
  const f = fixture();
  f.send
    .mockRejectedValueOnce(new Error('uncertain'))
    .mockResolvedValueOnce({ ...result, replayed: true });
  await f.screen.start();
  await f.screen.submit({ title: 'Original' });
  expect(f.screen.state.type).toBe('uncertain');
  await f.screen.submit({ title: 'Changed' });
  await f.screen.start();
  expect(f.send).toHaveBeenCalledTimes(1);
  f.screen.suspend();
  expect(f.screen.state.type).toBe('suspended');
  await f.screen.start();
  expect(f.screen.state.type).toBe('uncertain');
  expect(f.send).toHaveBeenCalledTimes(1);
  expect(f.access.mock.calls[1]?.[1]).toBe(id);
  await f.screen.retry();
  expect(f.send.mock.calls[1]?.slice(0, 4)).toEqual(
    f.send.mock.calls[0]?.slice(0, 4),
  );
  expect(f.screen.state.type).toBe('confirmed');
  expect(f.makeKey).toHaveBeenCalledTimes(1);
  f.screen.dispose();
});

it('hides pending commands synchronously and ignores completion after suspension or disposal', async () => {
  for (const action of ['suspend', 'dispose']) {
    const f = fixture();
    let finish: ((value: IssueCreationResult) => void) | undefined;
    f.send.mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    await f.screen.start();
    const sending = f.screen.submit({ title: 'Private' });
    if (action === 'suspend') {
      f.screen.suspend();
    } else {
      f.screen.dispose();
    }
    expect(f.screen.state.type).toBe('suspended');
    finish?.(result);
    await sending;
    expect(f.screen.state.type).toBe('suspended');
    await f.screen.start();
    expect(f.screen.state.type).toBe(
      action === 'suspend' ? 'uncertain' : 'suspended',
    );
    expect(f.send).toHaveBeenCalledTimes(1);
    f.screen.dispose();
  }
});

it('does not transfer a retained uncertain command to a changed identity or expose it after access failure', async () => {
  const f = fixture();
  f.send.mockRejectedValue(new Error('unknown'));
  await f.screen.start();
  await f.screen.submit({ title: 'Private' });
  f.screen.suspend();
  f.access.mockResolvedValueOnce(other);
  await f.screen.start();
  expect(f.screen.state.type).toBe('unavailable');
  await f.screen.retry();
  expect(f.send).toHaveBeenCalledTimes(1);
  f.access.mockRejectedValueOnce(new Error('access failed'));
  await f.screen.start();
  expect(f.screen.state.type).toBe('read-error');
  await f.screen.retry();
  expect(f.send).toHaveBeenCalledTimes(1);
  await f.screen.start();
  expect(f.screen.state.type).toBe('uncertain');
  f.screen.dispose();
});

it('revalidates references after confirmation and never resubmits on focus-style restart', async () => {
  const f = fixture();
  await f.screen.start();
  await f.screen.submit({ title: 'Private' });
  f.screen.suspend();
  await f.screen.start();
  expect(f.screen.state).toEqual({
    type: 'confirmed',
    receipt: result.receipt,
  });
  expect(f.send).toHaveBeenCalledTimes(1);
  expect(f.access).toHaveBeenCalledTimes(2);
  f.screen.dispose();
});

it('does not generate keys for invalid drafts or invalid routes and suppresses stale access completions', async () => {
  const f = fixture();
  await f.screen.start();
  await expect(f.screen.submit({ title: ' ' })).rejects.toThrow();
  expect(f.makeKey).not.toHaveBeenCalled();
  const invalid = new IssueCreationScreen(undefined, () => undefined, {
    access: f.access,
    send: f.send,
    key: f.makeKey,
  });
  await invalid.start();
  expect(invalid.state.type).toBe('unavailable');
  let finish: ((value: string) => void) | undefined;
  f.screen.suspend();
  f.access.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const starting = f.screen.start();
  f.screen.suspend();
  finish?.(id);
  await starting;
  expect(f.screen.state.type).toBe('suspended');
  f.screen.dispose();
  invalid.dispose();
});
