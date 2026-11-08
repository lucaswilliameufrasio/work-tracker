import { afterEach, expect, it, vi } from 'vitest';
import { getProjectMember, listProjectMembers } from './project-members.ts';

const project = '019a0000-0000-7000-8000-000000000001';
const firstUser = '019a0000-0000-7000-8000-000000000002';
const secondUser = '019a0000-0000-7000-8000-000000000003';
const sessionId = '019a0000-0000-7000-8000-000000000004';
const session = {
  user_id: project,
  session_id: sessionId,
  csrf_token: 'A'.repeat(43),
};
const member = {
  user_id: firstUser,
  display_name: 'Project member',
  role: 'member',
  can_be_assigned: true,
};
const signal = () => new AbortController().signal;

function json(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'Content-Type': 'application/json' },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it('reads a bounded roster page with same-origin credentials and verifies identity', async () => {
  const response = {
    data: [
      member,
      {
        user_id: secondUser,
        display_name: 'Project viewer',
        role: 'viewer',
        can_be_assigned: false,
        primary_email: 'viewer@example.test',
      },
    ],
    has_more: true,
    next_cursor: secondUser,
  };
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(response))
    .mockResolvedValueOnce(json(session));
  vi.stubGlobal('fetch', fetcher);
  await expect(
    listProjectMembers(project, project, { page_size: 2 }, signal()),
  ).resolves.toEqual(response);
  expect(fetcher).toHaveBeenCalledTimes(3);
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${project}/members?page_size=2`,
  );
  expect(fetcher.mock.calls[1]?.[1]).toMatchObject({
    method: 'GET',
    mode: 'same-origin',
    credentials: 'same-origin',
    cache: 'no-store',
    redirect: 'error',
  });
});

it('reads one existing assignee and rejects malformed or unauthorized member data', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(member))
    .mockResolvedValueOnce(json(session));
  vi.stubGlobal('fetch', fetcher);
  await expect(
    getProjectMember(project, firstUser, project, signal()),
  ).resolves.toEqual(member);
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${project}/members/${firstUser}`,
  );

  const malformed = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json({ ...member, can_be_assigned: true, role: 'viewer' }),
    )
    .mockResolvedValueOnce(json(session));
  vi.stubGlobal('fetch', malformed);
  await expect(
    getProjectMember(project, firstUser, project, signal()),
  ).rejects.toThrow('Invalid issue API response');
});

it('does not return roster data if the current session identity changed', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json({ data: [member], has_more: false }))
    .mockResolvedValueOnce(json({ ...session, user_id: secondUser }));
  vi.stubGlobal('fetch', fetcher);
  await expect(
    listProjectMembers(project, project, {}, signal()),
  ).rejects.toThrow('Project member identity changed');
});

it('rejects invalid cursors, oversized page choices, unsorted pages and bad cursors', async () => {
  await expect(
    listProjectMembers(project, project, { page_size: 101 }, signal()),
  ).rejects.toThrow('Invalid issue API response');
  await expect(
    listProjectMembers(project, project, { cursor: 'not-a-uuid' }, signal()),
  ).rejects.toThrow('Invalid issue API response');

  const unsorted = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json({
        data: [
          { ...member, user_id: secondUser },
          { ...member, user_id: firstUser },
        ],
        has_more: false,
      }),
    )
    .mockResolvedValueOnce(json(session));
  vi.stubGlobal('fetch', unsorted);
  await expect(
    listProjectMembers(project, project, { page_size: 2 }, signal()),
  ).rejects.toThrow('Invalid issue API response');
});
