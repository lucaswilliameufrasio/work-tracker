import {
  fields,
  invalid,
  issueFailure,
  issueJson,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';

export type IssueDueDateValue = string | null;
export type IssueDueDateInput = {
  expected_version: string;
  due_date: IssueDueDateValue;
};
export type IssueDueDateTarget = {
  id: string;
  issue_number: string;
  issue_key: string;
};
export type IssueDueDateReceipt = IssueDueDateTarget & {
  project_id: string;
  due_date: IssueDueDateValue;
  result_version: string;
  changed: boolean;
};
export type IssueDueDateResult = {
  receipt: IssueDueDateReceipt;
  replayed: boolean;
};

export class IssueDueDateIdentityChanged extends Error {
  constructor() {
    super('Issue due-date identity changed');
    this.name = 'IssueDueDateIdentityChanged';
  }
}

function decimal(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^[1-9][0-9]{0,18}$/.test(value) ||
    BigInt(value) > 9223372036854775807n
  ) {
    return invalid();
  }
  return value;
}

function date(value: unknown): IssueDueDateValue {
  if (value === null) {
    return null;
  }
  if (
    typeof value !== 'string' ||
    !/^\d{4}-\d{2}-\d{2}$/.test(value) ||
    !Number.isFinite(Date.parse(value)) ||
    new Date(value).toISOString().slice(0, 10) !== value
  ) {
    return invalid();
  }
  return value;
}

function target(value: IssueDueDateTarget): IssueDueDateTarget {
  const issue = {
    id: uuid(value.id),
    issue_number: decimal(value.issue_number),
    issue_key: value.issue_key,
  };
  if (
    typeof issue.issue_key !== 'string' ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(issue.issue_key) ||
    issue.issue_key.slice(issue.issue_key.indexOf('-') + 1) !==
      issue.issue_number
  ) {
    return invalid();
  }
  return issue;
}

export function serializeIssueDueDateInput(value: unknown): string {
  const item = fields(value, ['expected_version', 'due_date']);
  const encoded = JSON.stringify({
    expected_version: decimal(item.expected_version),
    due_date: date(item.due_date),
  });
  if (new TextEncoder().encode(encoded).length > 512 * 1024) {
    return invalid();
  }
  return encoded;
}

export function parseIssueDueDateReceipt(
  value: unknown,
  projectId: string,
  selectedTarget: IssueDueDateTarget,
  input: IssueDueDateInput,
): IssueDueDateReceipt {
  const item = fields(value, [
    'id',
    'project_id',
    'issue_number',
    'issue_key',
    'due_date',
    'result_version',
    'changed',
  ]);
  const expectedVersion = BigInt(decimal(input.expected_version));
  const selectedIssue = target(selectedTarget);
  const issueNumber = decimal(item.issue_number);
  const resultVersion = decimal(item.result_version);
  const resultDate = date(item.due_date);
  const selectedDate = date(input.due_date);
  if (
    uuid(item.project_id) !== uuid(projectId) ||
    uuid(item.id) !== selectedIssue.id ||
    issueNumber !== selectedIssue.issue_number ||
    resultDate !== selectedDate ||
    typeof item.changed !== 'boolean' ||
    BigInt(resultVersion) !== expectedVersion + (item.changed ? 1n : 0n) ||
    item.issue_key !== selectedIssue.issue_key ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(String(item.issue_key)) ||
    String(item.issue_key).slice(String(item.issue_key).indexOf('-') + 1) !==
      issueNumber
  ) {
    return invalid();
  }
  return {
    ...selectedIssue,
    project_id: uuid(item.project_id),
    due_date: resultDate,
    result_version: resultVersion,
    changed: item.changed,
  };
}

export async function setIssueDueDate(
  projectId: string,
  selectedTarget: IssueDueDateTarget,
  expectedActorId: string,
  idempotencyKey: string,
  input: IssueDueDateInput,
  signal: AbortSignal,
): Promise<IssueDueDateResult> {
  uuid(projectId);
  uuid(expectedActorId);
  uuid(idempotencyKey);
  const issue = target(selectedTarget);
  const selectedInput = fields(input, ['expected_version', 'due_date']);
  const body = serializeIssueDueDateInput(selectedInput);
  const expectedVersion = decimal(selectedInput.expected_version);
  const dueDate = date(selectedInput.due_date);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const session = await getCurrentSession(deadline);
  deadline.throwIfAborted();
  if (session.user_id !== expectedActorId) {
    throw new IssueDueDateIdentityChanged();
  }

  let response: Response;
  try {
    response = await fetch(
      `/api/v1/projects/${projectId}/issues/${issue.issue_number}/due-date`,
      {
        method: 'PUT',
        mode: 'same-origin',
        credentials: 'same-origin',
        cache: 'no-store',
        redirect: 'error',
        signal: deadline,
        headers: {
          Accept: 'application/json',
          'Content-Type': 'application/json',
          'x-csrf-token': session.csrf_token,
          'Idempotency-Key': idempotencyKey,
        },
        body,
      },
    );
  } catch {
    deadline.throwIfAborted();
    throw new Error('Issue due-date result could not be confirmed');
  }
  if (response.status !== 200) {
    return issueFailure(response, deadline);
  }
  const receipt = parseIssueDueDateReceipt(
    await issueJson(response, deadline, 4096),
    projectId,
    issue,
    { expected_version: expectedVersion, due_date: dueDate },
  );
  const replayed = response.headers.get('idempotency-replayed');
  if (
    (replayed !== 'true' && replayed !== 'false') ||
    response.headers.get('location') !==
      `/api/v1/projects/${projectId}/issues/${issue.issue_number}`
  ) {
    return invalid();
  }
  return { receipt, replayed: replayed === 'true' };
}
