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
  const searchCursor = btoa(`9007199254740993/${id}`)
    .replaceAll('+', '-')
    .replaceAll('/', '_')
    .replaceAll('=', '');
  expect(
    workRoute(
      `/projects/${id}/issues`,
      `?q=literal+terms&cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'search',
    projectId: id,
    query: 'literal terms',
    cursor: searchCursor,
  });
  expect(workRoute(`/projects/${id}/issues`, '?priority=urgent')).toEqual({
    type: 'search',
    projectId: id,
    priority: 'urgent',
  });
  expect(workRoute(`/projects/${id}/issues`, '?due_date=2026-10-30')).toEqual({
    type: 'search',
    projectId: id,
    dueDate: '2026-10-30',
  });
  const issueTypeId = '019a0000-0000-7000-8000-000000000099';
  expect(
    workRoute(`/projects/${id}/issues`, `?issue_type_id=${issueTypeId}`),
  ).toEqual({
    type: 'search',
    projectId: id,
    issueTypeId,
  });
  const statusId = '019a0000-0000-7000-8000-000000000098';
  expect(workRoute(`/projects/${id}/issues`, `?status_id=${statusId}`)).toEqual(
    {
      type: 'search',
      projectId: id,
      statusId,
    },
  );
  const assigneeId = '019a0000-0000-7000-8000-000000000096';
  expect(
    workRoute(`/projects/${id}/issues`, `?assignee_user_id=${assigneeId}`),
  ).toEqual({
    type: 'search',
    projectId: id,
    assigneeUserId: assigneeId,
  });
  expect(
    workRoute(
      `/projects/${id}/issues/1`,
      `?from_q=literal+terms&from_cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '1',
    fromSearchQuery: 'literal terms',
    fromSearchCursor: searchCursor,
  });
  expect(
    workRoute(
      `/projects/${id}/issues/1`,
      `?from_due_date=2026-10-30&from_cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '1',
    fromSearchDueDate: '2026-10-30',
    fromSearchCursor: searchCursor,
  });
  expect(
    workRoute(
      `/projects/${id}/issues/1`,
      `?from_issue_type_id=${issueTypeId}&from_cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '1',
    fromSearchIssueTypeId: issueTypeId,
    fromSearchCursor: searchCursor,
  });
  expect(
    workRoute(
      `/projects/${id}/issues/1`,
      `?from_status_id=${statusId}&from_cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '1',
    fromSearchStatusId: statusId,
    fromSearchCursor: searchCursor,
  });
  expect(
    workRoute(
      `/projects/${id}/issues/1`,
      `?from_assignee_user_id=${assigneeId}&from_cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '1',
    fromSearchAssigneeUserId: assigneeId,
    fromSearchCursor: searchCursor,
  });
  expect(
    workRoute(
      `/projects/${id}/issues/1`,
      `?from_priority=urgent&from_cursor=${searchCursor}`,
    ),
  ).toEqual({
    type: 'detail',
    projectId: id,
    number: '1',
    fromSearchPriority: 'urgent',
    fromSearchCursor: searchCursor,
  });
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
    [`/projects/${id}/issues`, '?q=%20'],
    [`/projects/${id}/issues`, '?q=one%0Atwo'],
    [`/projects/${id}/issues`, '?priority=invalid'],
    [`/projects/${id}/issues`, '?due_date='],
    [`/projects/${id}/issues`, '?due_date=2026-02-30'],
    [`/projects/${id}/issues/1`, '?from_due_date=2026-02-30'],
    [`/projects/${id}/issues`, '?issue_type_id='],
    [`/projects/${id}/issues`, '?issue_type_id=not-a-type'],
    [`/projects/${id}/issues/1`, '?from_issue_type_id=not-a-type'],
    [`/projects/${id}/issues`, '?status_id='],
    [`/projects/${id}/issues`, '?status_id=not-a-status'],
    [`/projects/${id}/issues/1`, '?from_status_id=not-a-status'],
    [`/projects/${id}/issues`, '?assignee_user_id='],
    [`/projects/${id}/issues`, '?assignee_user_id=not-an-assignee'],
    [`/projects/${id}/issues/1`, '?from_assignee_user_id=not-an-assignee'],
    [`/projects/${id}/issues`, '?q=one&cursor=bad'],
    [`/projects/${id}/issues/1`, '?history_cursor=bad'],
    [`/projects/${id}/issues/1`, '?from_cursor=bad'],
    [`/projects/${id}/issues/1`, '?from=1&from_q=search'],
    [`/projects/${id}/issues/1`, `?history_cursor=${'x'.repeat(90)}`],
  ]) {
    expect(workRoute(path ?? '', query ?? '')).toEqual({ type: 'invalid' });
  }
});

it('loads a project search page through the dedicated issue-search API without private identity overrides', async () => {
  const fetcher = vi.fn<typeof fetch>(async (input) => {
    if (String(input).includes('/auth/session')) {
      return Response.json({
        user_id: id,
        session_id: id,
        csrf_token: 'A'.repeat(43),
      });
    }
    return Response.json({ data: [], has_more: false });
  });
  vi.stubGlobal('fetch', fetcher);
  await expect(
    loadWork(
      { type: 'search', projectId: id, query: 'literal terms' },
      new AbortController().signal,
    ),
  ).resolves.toEqual({
    type: 'search',
    projectId: id,
    query: 'literal terms',
    page: { data: [], has_more: false },
    issueTypes: [],
    issueTypesIncomplete: false,
    statuses: [],
    statusesIncomplete: false,
    assignees: [],
    assigneesIncomplete: false,
  });
  expect(fetcher).toHaveBeenCalledTimes(7);
  expect(String(fetcher.mock.calls[0]?.[0])).toContain(
    `/api/v1/projects/${id}/issues/search?q=literal+terms&page_size=20`,
  );
  expect(String(fetcher.mock.calls[1]?.[0])).toContain(
    `/api/v1/projects/${id}/issue-types?page_size=100&include_archived=false`,
  );
  expect(String(fetcher.mock.calls[2]?.[0])).toContain(
    `/api/v1/projects/${id}/statuses?page_size=100&include_archived=false`,
  );
});

it('loads a priority-only project search without adding an empty text query', async () => {
  const fetcher = vi.fn<typeof fetch>(async (input) => {
    if (String(input).includes('/auth/session')) {
      return Response.json({
        user_id: id,
        session_id: id,
        csrf_token: 'A'.repeat(43),
      });
    }
    return Response.json({ data: [], has_more: false });
  });
  vi.stubGlobal('fetch', fetcher);
  await expect(
    loadWork(
      { type: 'search', projectId: id, priority: 'urgent' },
      new AbortController().signal,
    ),
  ).resolves.toEqual({
    type: 'search',
    projectId: id,
    priority: 'urgent',
    page: { data: [], has_more: false },
    issueTypes: [],
    issueTypesIncomplete: false,
    statuses: [],
    statusesIncomplete: false,
    assignees: [],
    assigneesIncomplete: false,
  });
  expect(fetcher).toHaveBeenCalledTimes(7);
  expect(String(fetcher.mock.calls[0]?.[0])).toContain(
    `/api/v1/projects/${id}/issues/search?priority=urgent&page_size=20`,
  );
});

it('loads an exact due-date-only project search without adding empty criteria', async () => {
  const fetcher = vi.fn<typeof fetch>(async (input) => {
    if (String(input).includes('/auth/session')) {
      return Response.json({
        user_id: id,
        session_id: id,
        csrf_token: 'A'.repeat(43),
      });
    }
    return Response.json({ data: [], has_more: false });
  });
  vi.stubGlobal('fetch', fetcher);
  await expect(
    loadWork(
      { type: 'search', projectId: id, dueDate: '2026-10-30' },
      new AbortController().signal,
    ),
  ).resolves.toEqual({
    type: 'search',
    projectId: id,
    dueDate: '2026-10-30',
    page: { data: [], has_more: false },
    issueTypes: [],
    issueTypesIncomplete: false,
    statuses: [],
    statusesIncomplete: false,
    assignees: [],
    assigneesIncomplete: false,
  });
  expect(fetcher).toHaveBeenCalledTimes(7);
  expect(String(fetcher.mock.calls[0]?.[0])).toContain(
    `/api/v1/projects/${id}/issues/search?due_date=2026-10-30&page_size=20`,
  );
});

it('keeps an archived selected issue type visible without listing other archived types', async () => {
  const archivedId = '019a0000-0000-7000-8000-000000000099';
  const activeId = '019a0000-0000-7000-8000-000000000098';
  const fetcher = vi.fn<typeof fetch>(async (input) => {
    const url = String(input);
    if (url.includes('/auth/session')) {
      return Response.json({
        user_id: id,
        session_id: id,
        csrf_token: 'A'.repeat(43),
      });
    }
    if (url.includes('/issues/search?')) {
      return Response.json({ data: [], has_more: false });
    }
    if (url.includes(`/issue-types/${archivedId}`)) {
      return Response.json({
        id: archivedId,
        project_id: id,
        name: 'Defeito',
        archived_at: '2026-10-24T00:00:00.000000000Z',
      });
    }
    if (url.includes('/issue-types?')) {
      return Response.json({
        data: [{ id: activeId, project_id: id, name: 'Tarefa' }],
        has_more: false,
      });
    }
    return Response.json({ data: [], has_more: false });
  });
  vi.stubGlobal('fetch', fetcher);
  await expect(
    loadWork(
      { type: 'search', projectId: id, issueTypeId: archivedId },
      new AbortController().signal,
    ),
  ).resolves.toEqual({
    type: 'search',
    projectId: id,
    issueTypeId: archivedId,
    page: { data: [], has_more: false },
    issueTypesIncomplete: false,
    statuses: [],
    statusesIncomplete: false,
    assignees: [],
    assigneesIncomplete: false,
    issueTypes: [
      { id: activeId, project_id: id, name: 'Tarefa' },
      {
        id: archivedId,
        project_id: id,
        name: 'Defeito',
        archived_at: '2026-10-24T00:00:00.000000000Z',
      },
    ],
  });
  expect(String(fetcher.mock.calls[0]?.[0])).toContain(
    `issue_type_id=${archivedId}`,
  );
  expect(
    fetcher.mock.calls.some((call) =>
      String(call[0]).includes('include_archived=true'),
    ),
  ).toBe(false);
});

it('keeps an ineligible selected assignee visible without listing other ineligible members', async () => {
  const assigneeId = '019a0000-0000-7000-8000-000000000096';
  const eligibleId = '019a0000-0000-7000-8000-000000000095';
  const fetcher = vi.fn<typeof fetch>(async (input) => {
    const url = String(input);
    if (url.includes('/auth/session')) {
      return Response.json({
        user_id: id,
        session_id: id,
        csrf_token: 'A'.repeat(43),
      });
    }
    if (url.includes('/issues/search?')) {
      return Response.json({ data: [], has_more: false });
    }
    if (url.includes(`/members/${assigneeId}`)) {
      return Response.json({
        user_id: assigneeId,
        display_name: 'Nara',
        role: 'viewer',
        can_be_assigned: false,
      });
    }
    if (url.includes('/members?')) {
      return Response.json({
        data: [
          {
            user_id: '019a0000-0000-7000-8000-000000000094',
            display_name: 'Leitor',
            role: 'viewer',
            can_be_assigned: false,
          },
          {
            user_id: eligibleId,
            display_name: 'Ada',
            role: 'member',
            can_be_assigned: true,
          },
        ],
        has_more: false,
      });
    }
    return Response.json({ data: [], has_more: false });
  });
  vi.stubGlobal('fetch', fetcher);
  await expect(
    loadWork(
      { type: 'search', projectId: id, assigneeUserId: assigneeId },
      new AbortController().signal,
    ),
  ).resolves.toEqual({
    type: 'search',
    projectId: id,
    assigneeUserId: assigneeId,
    page: { data: [], has_more: false },
    issueTypes: [],
    issueTypesIncomplete: false,
    statuses: [],
    statusesIncomplete: false,
    assigneesIncomplete: false,
    assignees: [
      { user_id: eligibleId, display_name: 'Ada', eligible: true },
      { user_id: assigneeId, display_name: 'Nara', eligible: false },
    ],
  });
  expect(String(fetcher.mock.calls[0]?.[0])).toContain(
    `assignee_user_id=${assigneeId}`,
  );
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
    if (path.includes('/capabilities')) {
      return Response.json({
        project_id: id,
        can_create_issues: true,
        can_update_issue_fields: true,
        can_replace_issue_text: true,
        can_move_issues: true,
      });
    }
    if (path === '/api/v1/auth/session') {
      return Response.json({
        user_id: id,
        session_id: '019a0000-0000-7000-8000-000000000003',
        csrf_token: 'A'.repeat(43),
      });
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
      is_overdue: false,
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
  expect(fetcher).toHaveBeenCalledTimes(6);
});
