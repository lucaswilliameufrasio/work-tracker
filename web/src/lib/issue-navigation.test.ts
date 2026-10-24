import { afterEach, expect, it, vi } from 'vitest';
import {
  WorkScreen,
  loadWork,
  workRoute,
  type WorkData,
} from './issue-navigation';
import { IssueRequestError } from './issues';
import { getProjectPage, parseProjectPage } from './projects';

const id = '019a0000-0000-7000-8000-000000000001';
const project = { id, project_key: 'DEV', name: 'Work', role: 'viewer' };
const empty: WorkData = {
  type: 'projects',
  page: { data: [], has_more: false },
};
afterEach(() => vi.unstubAllGlobals());

it('strictly routes project, issue, detail, history and return boundaries without accepting identity overrides', () => {
  expect(workRoute('/projects', '?cursor=DEV&include_archived=true')).toEqual({
    type: 'projects',
    cursor: 'DEV',
    includeArchived: true,
  });
  expect(
    workRoute(`/projects/${id}/issues`, '?cursor=9007199254740993'),
  ).toEqual({ type: 'issues', projectId: id, cursor: '9007199254740993' });
  expect(
    workRoute(`/projects/${id}/issues/9223372036854775807`, '?from=1'),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '9223372036854775807',
    from: '1',
  });
  for (const [path, query] of [
    ['/projects', '?cursor=a'],
    ['/projects', '?include_archived=1'],
    ['/projects', '?cursor=AB&cursor=CD'],
    ['/projects', '?user_id=other'],
    [`/projects/${id}/issues/0`, ''],
    [`/projects/${id}/issues/01`, ''],
    [`/projects/${id}/issues/9223372036854775808`, ''],
    [`/projects/${id}/issues/1/history`, ''],
    ['/projects/bad/issues', ''],
    [`/projects/${id}/issues`, '?cursor=0'],
    [`/projects/${id}/issues/1`, '?history_cursor=bad'],
    [`/projects/${id}/issues/1`, `?history_cursor=${'x'.repeat(90)}`],
  ]) {
    expect(workRoute(path ?? '', query ?? '')).toEqual({ type: 'invalid' });
  }
});

it('validates accessible project pages without operational metadata or silent archive inclusion', () => {
  expect(parseProjectPage({ data: [project], has_more: false })).toEqual({
    data: [project],
    has_more: false,
  });
  for (const raw of [
    { data: [{ ...project, issue_count: 9 }], has_more: false },
    { data: [{ ...project, role: 'owner' }], has_more: false },
    { data: [{ ...project, id: 'bad' }], has_more: false },
    { data: [{ ...project, archived_at: null }], has_more: false },
    {
      data: [{ ...project, archived_at: '2026-10-21T00:00:00.000000000Z' }],
      has_more: false,
    },
    { data: [], has_more: true, next_cursor: 'DEV' },
    { data: [project, project], has_more: false },
    { data: [project], has_more: false, next_cursor: null },
  ]) {
    expect(() => parseProjectPage(raw)).toThrow();
  }
  expect(() =>
    parseProjectPage({ data: [project], has_more: false }, 'DEV'),
  ).toThrow();
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValue(Response.json({ data: [project], has_more: false }));
  vi.stubGlobal('fetch', fetcher);
  return expect(
    getProjectPage(undefined, false, new AbortController().signal),
  ).resolves.toEqual({ data: [project], has_more: false });
});

it('invalid route makes no API call and expired, denied and transport errors retain no private data', async () => {
  const invalidLoad = vi.fn<typeof loadWork>();
  const invalid = new WorkScreen(
    { type: 'invalid' },
    () => undefined,
    invalidLoad,
  );
  await invalid.start();
  expect(invalid.state.type).toBe('unavailable');
  expect(invalidLoad).not.toHaveBeenCalled();
  for (const [error, state] of [
    [new IssueRequestError(401, 'unauthenticated', id, false), 'signed-out'],
    [new IssueRequestError(404, 'not_found', id, false), 'unavailable'],
    [new IssueRequestError(403, 'permission_denied', id, false), 'unavailable'],
    [new Error('PRIVATE'), 'read-error'],
  ]) {
    const api = vi
      .fn<typeof loadWork>()
      .mockResolvedValueOnce(empty)
      .mockRejectedValueOnce(error);
    const screen = new WorkScreen(
      { type: 'projects', includeArchived: false },
      () => undefined,
      api,
    );
    await screen.start();
    expect(screen.state.type).toBe('ready');
    await screen.refresh();
    expect(screen.state).toEqual({ type: state });
  }
});

it('synchronously suspends private data, ignores obsolete results and coalesces restore reads', async () => {
  let finish: ((value: WorkData) => void) | undefined;
  const api = vi
    .fn<typeof loadWork>()
    .mockImplementationOnce(
      async () =>
        new Promise<WorkData>((resolve) => {
          finish = resolve;
        }),
    )
    .mockResolvedValue(empty);
  const screen = new WorkScreen(
    { type: 'projects', includeArchived: false },
    () => undefined,
    api,
  );
  const pending = screen.start();
  screen.suspend();
  expect(screen.state).toEqual({ type: 'suspended' });
  const resumed = screen.start();
  await screen.start();
  expect(api).toHaveBeenCalledTimes(2);
  await resumed;
  finish?.({
    type: 'projects',
    page: { data: [{ ...project, role: 'viewer' }], has_more: false },
  });
  await pending;
  expect(screen.state).toEqual({ type: 'ready', data: empty });
  screen.suspend();
  expect(screen.state).toEqual({ type: 'suspended' });
  screen.dispose();
  await screen.start();
  expect(screen.state).toEqual({ type: 'suspended' });
  expect(api).toHaveBeenCalledTimes(2);
});

it('synchronously signs out on a shared session-ended signal and ignores pending private reads', async () => {
  let finish: ((value: WorkData) => void) | undefined;
  const api = vi.fn<typeof loadWork>().mockImplementation(
    async () =>
      new Promise<WorkData>((resolve) => {
        finish = resolve;
      }),
  );
  const screen = new WorkScreen(
    { type: 'projects', includeArchived: false },
    () => undefined,
    api,
  );
  const pending = screen.start();
  const signal = api.mock.calls[0]?.[1];
  screen.sessionEnded();
  expect(signal?.aborted).toBe(true);
  expect(screen.state).toEqual({ type: 'signed-out' });
  finish?.(empty);
  await pending;
  expect(screen.state).toEqual({ type: 'signed-out' });
});

it('dispose cancels pending work and stale callbacks cannot repopulate private state', async () => {
  let finish: ((value: WorkData) => void) | undefined;
  const api = vi.fn<typeof loadWork>().mockImplementation(
    async () =>
      new Promise<WorkData>((resolve) => {
        finish = resolve;
      }),
  );
  const changed = vi.fn();
  const screen = new WorkScreen(
    { type: 'projects', includeArchived: false },
    changed,
    api,
  );
  const pending = screen.start();
  const signal = api.mock.calls[0]?.[1];
  screen.dispose();
  expect(signal?.aborted).toBe(true);
  finish?.(empty);
  await pending;
  expect(changed).toHaveBeenCalledTimes(1);
});

it('project pagination requires exact full-page progress and preserves explicit archived records', () => {
  const projects = Array.from({ length: 20 }, (_, index) => ({
    ...project,
    id: `${id.slice(0, -2)}${index.toString(16).padStart(2, '0')}`,
    project_key: `A${String.fromCharCode(65 + index)}`,
  }));
  const raw = { data: projects, has_more: true, next_cursor: 'AT' };
  expect(parseProjectPage(raw).next_cursor).toBe('AT');
  expect(() => parseProjectPage({ ...raw, next_cursor: 'AU' })).toThrow();
  const archived = {
    ...project,
    archived_at: '2026-10-21T00:00:00.000000000Z',
  };
  expect(
    parseProjectPage({ data: [archived], has_more: false }, undefined, true)
      .data[0],
  ).toEqual(archived);
});

it('composite detail rejects a journal from a different issue even in the same project', async () => {
  const time = '2026-10-21T00:00:00.000000000Z';
  const fetcher = vi.fn<typeof fetch>().mockImplementation(async (path) => {
    if (typeof path !== 'string') {
      throw new Error('Expected fixed path');
    }
    if (path.includes('/history?')) {
      return Response.json({
        data: [
          {
            id,
            project_id: id,
            issue_id: '019a0000-0000-7000-8000-000000000002',
            actor_user_id: id,
            event_kind: 'issue.created',
            occurred_at: time,
          },
        ],
        has_more: false,
      });
    }
    if (path.includes('/statuses/')) {
      return Response.json({
        id,
        project_id: id,
        name: 'Status',
        category: 'active',
        position: '0',
        board_visible: true,
      });
    }
    if (path.includes('/issue-types/')) {
      return Response.json({ id, project_id: id, name: 'Type' });
    }
    return Response.json({
      id,
      project_id: id,
      issue_number: '1',
      issue_key: 'DEV-1',
      issue_type_id: id,
      status_id: id,
      title: 'Detail',
      creator_user_id: id,
      priority: 'none',
      version: '1',
      created_at: time,
      updated_at: time,
    });
  });
  vi.stubGlobal('fetch', fetcher);
  await expect(
    loadWork(
      { type: 'detail', projectId: id, number: '1' },
      new AbortController().signal,
    ),
  ).rejects.toThrow('Invalid issue API response');
  expect(fetcher).toHaveBeenCalledTimes(4);
});
