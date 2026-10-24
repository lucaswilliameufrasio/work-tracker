import { afterEach, expect, it, vi } from 'vitest';
import {
  parseIssueAssigneeReceipt,
  serializeIssueAssigneeInput,
  setIssueAssignee,
} from './issue-assignee.ts';

const project = '019a0000-0000-7000-8000-000000000001';
const issueId = '019a0000-0000-7000-8000-000000000002';
const sessionId = '019a0000-0000-7000-8000-000000000003';
const assigneeId = '019a0000-0000-7000-8000-000000000004';
const key = '019a0000-0000-7000-8000-000000000005';
const target = {
  id: issueId,
  issue_number: '9007199254740993',
  issue_key: 'DEV-9007199254740993',
};
const input = { expected_version: '1', assignee_user_id: assigneeId };
const session = {
  user_id: project,
  session_id: sessionId,
  csrf_token: 'A'.repeat(43),
};
const receipt = {
  id: issueId,
  project_id: project,
  issue_number: target.issue_number,
  issue_key: target.issue_key,
  assignee_user_id: assigneeId,
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

it('serializes one assignment field and requires explicit null to clear', () => {
  expect(serializeIssueAssigneeInput(input)).toBe(
    `{"expected_version":"1","assignee_user_id":"${assigneeId}"}`,
  );
  expect(
    serializeIssueAssigneeInput({
      expected_version: '2',
      assignee_user_id: null,
    }),
  ).toBe('{"expected_version":"2","assignee_user_id":null}');
  for (const value of [
    null,
    {},
    { expected_version: 1, assignee_user_id: assigneeId },
    { expected_version: '01', assignee_user_id: assigneeId },
    { expected_version: '1', assignee_user_id: 'bad-id' },
    { ...input, priority: 'high' },
  ]) {
    expect(() => serializeIssueAssigneeInput(value)).toThrow();
  }
});

it('validates assignment and clear receipts while preserving decimal precision', () => {
  expect(parseIssueAssigneeReceipt(receipt, project, target, input)).toEqual(
    receipt,
  );
  const clear = {
    ...receipt,
    assignee_user_id: null,
    result_version: '1',
    changed: false,
  };
  expect(
    parseIssueAssigneeReceipt(clear, project, target, {
      expected_version: '1',
      assignee_user_id: null,
    }),
  ).toEqual(clear);
  for (const value of [
    null,
    {},
    { ...receipt, title: 'private text' },
    { ...receipt, id: sessionId },
    { ...receipt, project_id: sessionId },
    { ...receipt, assignee_user_id: 'not-a-uuid' },
    { ...receipt, result_version: '1' },
    { ...receipt, changed: false },
    { ...receipt, changed: 'true' },
  ]) {
    expect(() =>
      parseIssueAssigneeReceipt(value, project, target, input),
    ).toThrow();
  }
});

it('checks actor identity then sends a same-origin assignment command', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(receipt, 200, successHeaders(true)));
  vi.stubGlobal('fetch', fetcher);
  const result = await setIssueAssignee(
    project,
    target,
    project,
    key,
    input,
    signal(),
  );
  expect(result).toEqual({ receipt, replayed: true });
  expect(fetcher).toHaveBeenCalledTimes(2);
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${project}/issues/${target.issue_number}/assignee`,
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

it('does not write across actor identity changes or accept a mismatched receipt location', async () => {
  const changedActor = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json({ ...session, user_id: sessionId }));
  vi.stubGlobal('fetch', changedActor);
  await expect(
    setIssueAssignee(project, target, project, key, input, signal()),
  ).rejects.toThrow('Issue assignee identity changed');
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
    setIssueAssignee(project, target, project, key, input, signal()),
  ).rejects.toThrow('Invalid issue API response');
});
