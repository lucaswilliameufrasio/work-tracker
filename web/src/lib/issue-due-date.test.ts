import { afterEach, expect, it, vi } from 'vitest';
import {
  parseIssueDueDateReceipt,
  serializeIssueDueDateInput,
  setIssueDueDate,
} from './issue-due-date.ts';

const project = '019a0000-0000-7000-8000-000000000001';
const issue = '019a0000-0000-7000-8000-000000000002';
const sessionId = '019a0000-0000-7000-8000-000000000003';
const key = '019a0000-0000-7000-8000-000000000004';
const target = {
  id: issue,
  issue_number: '9007199254740993',
  issue_key: 'DEV-9007199254740993',
};
const input = { expected_version: '1', due_date: '2028-02-29' };
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
  due_date: '2028-02-29',
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

it('serializes exactly one civil due-date field, including explicit null to clear', () => {
  expect(serializeIssueDueDateInput(input)).toBe(
    '{"expected_version":"1","due_date":"2028-02-29"}',
  );
  expect(
    serializeIssueDueDateInput({ expected_version: '2', due_date: null }),
  ).toBe('{"expected_version":"2","due_date":null}');
  for (const value of [
    null,
    {},
    { expected_version: 1, due_date: '2028-01-01' },
    { expected_version: '01', due_date: '2028-01-01' },
    { expected_version: '1', due_date: '2026-02-29' },
    { expected_version: '1', due_date: '2028-2-09' },
    { ...input, priority: 'high' },
  ]) {
    expect(() => serializeIssueDueDateInput(value)).toThrow();
  }
});

it('validates date receipts, clear receipts, and lossless version results', () => {
  expect(parseIssueDueDateReceipt(receipt, project, target, input)).toEqual(
    receipt,
  );
  const clear = {
    ...receipt,
    due_date: null,
    result_version: '1',
    changed: false,
  };
  expect(
    parseIssueDueDateReceipt(clear, project, target, {
      expected_version: '1',
      due_date: null,
    }),
  ).toEqual(clear);
  for (const value of [
    null,
    {},
    { ...receipt, title: 'private text' },
    { ...receipt, id: sessionId },
    { ...receipt, project_id: sessionId },
    { ...receipt, due_date: '2026-02-29' },
    { ...receipt, result_version: '1' },
    { ...receipt, changed: false },
    { ...receipt, changed: 'true' },
  ]) {
    expect(() =>
      parseIssueDueDateReceipt(value, project, target, input),
    ).toThrow();
  }
});

it('checks actor identity then sends a same-origin due-date command with replay metadata', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(json(receipt, 200, successHeaders(true)));
  vi.stubGlobal('fetch', fetcher);
  const result = await setIssueDueDate(
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
    `/api/v1/projects/${project}/issues/${target.issue_number}/due-date`,
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

it('does not send a write for changed identity and does not accept a mismatched location', async () => {
  const changedActor = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json({ ...session, user_id: sessionId }));
  vi.stubGlobal('fetch', changedActor);
  await expect(
    setIssueDueDate(project, target, project, key, input, signal()),
  ).rejects.toThrow('Issue due-date identity changed');
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
    setIssueDueDate(project, target, project, key, input, signal()),
  ).rejects.toThrow('Invalid issue API response');
});
