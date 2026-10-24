import { afterEach, expect, it, vi } from 'vitest';
import {
  moveIssue,
  parseIssueMoveReceipt,
  serializeIssueMoveInput,
} from './issue-move';
import { onSessionEnded } from './session-lifecycle';

const project = '019a0000-0000-7000-8000-000000000001';
const issue = '019a0000-0000-7000-8000-000000000002';
const status = '019a0000-0000-7000-8000-000000000003';
const next = '019a0000-0000-7000-8000-000000000004';
const key = '019a0000-0000-7000-8000-000000000005';
const target = { id: issue, issue_number: '9007199254740993' };
const receipt = {
  id: issue,
  project_id: project,
  issue_number: target.issue_number,
  issue_key: `DEV-${target.issue_number}`,
  status_id: status,
  result_version: '2',
  changed: true,
};
const session = {
  user_id: project,
  session_id: next,
  csrf_token: 'A'.repeat(43),
};
const input = {
  expected_version: '1',
  target_status_id: status,
  next_issue_id: next,
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

function successHeaders(): Record<string, string> {
  return {
    Location: `/api/v1/projects/${project}/issues/${target.issue_number}`,
    'Idempotency-Replayed': 'false',
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it('serializes only the version, target status and optional neighbors as UUIDs', () => {
  expect(serializeIssueMoveInput(input)).toBe(
    `{"expected_version":"1","target_status_id":"${status}","next_issue_id":"${next}"}`,
  );
  expect(
    serializeIssueMoveInput({
      expected_version: '1',
      target_status_id: status,
    }),
  ).toBe(`{"expected_version":"1","target_status_id":"${status}"}`);
  for (const value of [
    null,
    {},
    { target_status_id: status },
    { ...input, expected_version: 1 },
    { ...input, expected_version: '01' },
    { ...input, expected_version: '9223372036854775808' },
    { ...input, previous_issue_id: null },
    { ...input, rank: '10' },
    { ...input, previous_issue_id: next },
    { ...input, target_status_id: 'bad' },
  ]) {
    expect(() => serializeIssueMoveInput(value)).toThrow();
  }
});

it('validates content-free receipts and preserves decimal i64 values losslessly', () => {
  expect(parseIssueMoveReceipt(receipt, project, target, status, '1')).toEqual(
    receipt,
  );
  expect(
    parseIssueMoveReceipt(
      { ...receipt, changed: false, result_version: '1' },
      project,
      target,
      status,
      '1',
    ).changed,
  ).toBe(false);
  expect(
    parseIssueMoveReceipt(
      { ...receipt, result_version: '9007199254740994' },
      project,
      target,
      status,
      '9007199254740993',
    ).result_version,
  ).toBe('9007199254740994');
  for (const value of [
    null,
    {},
    { ...receipt, prior_title: 'private' },
    { ...receipt, project_id: next },
    { ...receipt, status_id: next },
    { ...receipt, issue_number: '1' },
    { ...receipt, issue_key: 'DEV-2' },
    { ...receipt, result_version: '1' },
    { ...receipt, changed: false },
    { ...receipt, changed: 'true' },
    { ...receipt, result_version: '03' },
    { ...receipt, result_version: '9223372036854775808' },
  ]) {
    expect(() =>
      parseIssueMoveReceipt(value, project, target, status, '1'),
    ).toThrow();
  }
});

it('checks the current actor and posts one fixed same-origin request with the exact key', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(receipt, 200, successHeaders()));
  vi.stubGlobal('fetch', fetcher);
  const result = await moveIssue(
    project,
    target,
    project,
    key,
    input,
    signal(),
  );
  expect(result).toEqual({ receipt, replayed: false });
  expect(fetcher).toHaveBeenCalledTimes(2);
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${project}/issues/${target.issue_number}/move`,
  );
  expect(fetcher.mock.calls[1]?.[1]).toMatchObject({
    method: 'POST',
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
  expect(fetcher.mock.calls[0]?.[0]).toBe('/api/v1/auth/session');
});

it('does not send for another actor, exposes only safe HTTP errors and notifies on 401', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json(
        {
          code: 'issue_order_conflict',
          message: 'private server detail',
          request_id: next,
          field_errors: {},
          retryable: false,
        },
        409,
      ),
    );
  vi.stubGlobal('fetch', fetcher);
  await expect(
    moveIssue(project, target, next, key, input, signal()),
  ).rejects.toThrow('Issue move identity changed');
  expect(fetcher).toHaveBeenCalledTimes(1);

  await expect(
    moveIssue(project, target, project, key, input, signal()),
  ).rejects.toMatchObject({
    status: 409,
    code: 'issue_order_conflict',
  });
  expect(fetcher).toHaveBeenCalledTimes(3);

  const notify = vi.fn();
  const unsubscribe = onSessionEnded(notify);
  const unauthorized = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json(
        {
          code: 'unauthenticated',
          message: 'private server detail',
          request_id: next,
          field_errors: {},
          retryable: false,
        },
        401,
      ),
    );
  vi.stubGlobal('fetch', unauthorized);
  await expect(
    moveIssue(project, target, project, key, input, signal()),
  ).rejects.toMatchObject({ status: 401 });
  expect(notify).toHaveBeenCalledOnce();
  unsubscribe();
});
