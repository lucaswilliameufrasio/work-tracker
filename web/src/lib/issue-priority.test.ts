import { afterEach, expect, it, vi } from 'vitest';
import {
  parseIssuePriorityReceipt,
  serializeIssuePriorityInput,
  setIssuePriority,
} from './issue-priority.ts';

const project = '019a0000-0000-7000-8000-000000000001';
const issue = '019a0000-0000-7000-8000-000000000002';
const sessionId = '019a0000-0000-7000-8000-000000000003';
const key = '019a0000-0000-7000-8000-000000000004';
const target = {
  id: issue,
  issue_number: '9007199254740993',
  issue_key: 'DEV-9007199254740993',
};
const input = { expected_version: '1', priority: 'high' as const };
const session = {
  user_id: project,
  session_id: sessionId,
  csrf_token: 'A'.repeat(43),
};
const receipt = {
  id: issue,
  project_id: project,
  issue_number: target.issue_number,
  issue_key: target.issue_key,
  priority: 'high',
  result_version: '2',
  changed: true,
};
const signal = () => new AbortController().signal;

function json(
  value: unknown,
  statusCode = 200,
  extra: Record<string, string> = {},
): Response {
  return new Response(JSON.stringify(value), {
    status: statusCode,
    headers: { 'Content-Type': 'application/json', ...extra },
  });
}

function successHeaders(replayed = false): Record<string, string> {
  return {
    Location: `/api/v1/projects/${project}/issues/${target.issue_number}`,
    'Idempotency-Replayed': String(replayed),
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it('serializes one selected structured field and rejects malformed or multi-field input', () => {
  expect(serializeIssuePriorityInput(input)).toBe(
    '{"expected_version":"1","priority":"high"}',
  );
  for (const value of [
    null,
    {},
    { expected_version: 1, priority: 'high' },
    { expected_version: '01', priority: 'high' },
    { expected_version: '9223372036854775808', priority: 'high' },
    { expected_version: '1', priority: 'invalid' },
    { ...input, due_date: '2028-01-01' },
    { ...input, assignee_user_id: issue },
  ]) {
    expect(() => serializeIssuePriorityInput(value)).toThrow();
  }
});

it('validates a content-free receipt and preserves decimal i64 values losslessly', () => {
  expect(parseIssuePriorityReceipt(receipt, project, target, input)).toEqual(
    receipt,
  );
  expect(
    parseIssuePriorityReceipt(
      { ...receipt, changed: false, result_version: '1' },
      project,
      target,
      input,
    ).result_version,
  ).toBe('1');
  for (const value of [
    null,
    {},
    { ...receipt, title: 'private text' },
    { ...receipt, id: sessionId },
    { ...receipt, project_id: sessionId },
    { ...receipt, issue_key: 'DEV-2' },
    { ...receipt, priority: 'bad' },
    { ...receipt, result_version: '1' },
    { ...receipt, changed: false },
    { ...receipt, changed: 'true' },
    { ...receipt, result_version: '01' },
    { ...receipt, result_version: '9223372036854775808' },
  ]) {
    expect(() =>
      parseIssuePriorityReceipt(value, project, target, input),
    ).toThrow();
  }
});

it('checks current identity and sends one fixed same-origin request with exact replay metadata', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(receipt, 200, successHeaders(true)));
  vi.stubGlobal('fetch', fetcher);
  const result = await setIssuePriority(
    project,
    target,
    project,
    key,
    input,
    signal(),
  );
  expect(result).toEqual({ receipt, replayed: true });
  expect(fetcher).toHaveBeenCalledTimes(2);
  expect(fetcher.mock.calls[0]?.[0]).toBe('/api/v1/auth/session');
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${project}/issues/${target.issue_number}/priority`,
  );
  expect(fetcher.mock.calls[1]?.[1]).toMatchObject({
    method: 'PUT',
    mode: 'same-origin',
    credentials: 'same-origin',
    cache: 'no-store',
    redirect: 'error',
    headers: {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      'x-csrf-token': session.csrf_token,
      'Idempotency-Key': key,
    },
    body: JSON.stringify(input),
  });
});

it('does not send after an actor change and rejects a receipt with mismatched resource headers', async () => {
  const invalidTargetFetcher = vi.fn<typeof fetch>();
  vi.stubGlobal('fetch', invalidTargetFetcher);
  await expect(
    setIssuePriority(
      project,
      { ...target, issue_key: 'DEV-2' },
      project,
      key,
      input,
      signal(),
    ),
  ).rejects.toThrow('Invalid issue API response');
  expect(invalidTargetFetcher).not.toHaveBeenCalled();

  const changedActor = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json({ ...session, user_id: sessionId }));
  vi.stubGlobal('fetch', changedActor);
  await expect(
    setIssuePriority(project, target, project, key, input, signal()),
  ).rejects.toThrow('Issue priority identity changed');
  expect(changedActor).toHaveBeenCalledTimes(1);

  const badLocation = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json(receipt, 200, {
        ...successHeaders(),
        Location: `/api/v1/projects/${project}/issues/1`,
      }),
    );
  vi.stubGlobal('fetch', badLocation);
  await expect(
    setIssuePriority(project, target, project, key, input, signal()),
  ).rejects.toThrow('Invalid issue API response');
});
