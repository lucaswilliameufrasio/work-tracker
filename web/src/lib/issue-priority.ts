import {
  fields,
  invalid,
  issueFailure,
  issueJson,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';
import type { IssuePriority } from './issues.ts';

export type IssuePriorityInput = {
  expected_version: string;
  priority: IssuePriority;
};
export type IssuePriorityTarget = {
  id: string;
  issue_number: string;
  issue_key: string;
};
export type IssuePriorityReceipt = IssuePriorityTarget & {
  project_id: string;
  priority: IssuePriority;
  result_version: string;
  changed: boolean;
};
export type IssuePriorityResult = {
  receipt: IssuePriorityReceipt;
  replayed: boolean;
};

export class IssuePriorityIdentityChanged extends Error {
  constructor() {
    super('Issue priority identity changed');
    this.name = 'IssuePriorityIdentityChanged';
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

function priority(value: unknown): IssuePriority {
  if (
    value !== 'none' &&
    value !== 'low' &&
    value !== 'medium' &&
    value !== 'high' &&
    value !== 'urgent'
  ) {
    return invalid();
  }
  return value;
}

export function serializeIssuePriorityInput(value: unknown): string {
  const item = fields(value, ['expected_version', 'priority']);
  const encoded = JSON.stringify({
    expected_version: decimal(item.expected_version),
    priority: priority(item.priority),
  });
  if (new TextEncoder().encode(encoded).length > 512 * 1024) {
    return invalid();
  }
  return encoded;
}

export function parseIssuePriorityReceipt(
  value: unknown,
  projectId: string,
  target: IssuePriorityTarget,
  input: IssuePriorityInput,
): IssuePriorityReceipt {
  const item = fields(value, [
    'id',
    'project_id',
    'issue_number',
    'issue_key',
    'priority',
    'result_version',
    'changed',
  ]);
  const expectedVersion = BigInt(decimal(input.expected_version));
  const issueNumber = decimal(item.issue_number);
  const resultVersion = decimal(item.result_version);
  if (
    uuid(item.project_id) !== uuid(projectId) ||
    uuid(item.id) !== uuid(target.id) ||
    issueNumber !== decimal(target.issue_number) ||
    priority(item.priority) !== priority(input.priority) ||
    typeof item.changed !== 'boolean' ||
    BigInt(resultVersion) !== expectedVersion + (item.changed ? 1n : 0n) ||
    typeof item.issue_key !== 'string' ||
    item.issue_key !== target.issue_key ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(item.issue_key) ||
    item.issue_key.slice(item.issue_key.indexOf('-') + 1) !== issueNumber
  ) {
    return invalid();
  }
  return {
    id: uuid(item.id),
    project_id: uuid(item.project_id),
    issue_number: issueNumber,
    issue_key: item.issue_key,
    priority: priority(item.priority),
    result_version: resultVersion,
    changed: item.changed,
  };
}

export async function setIssuePriority(
  projectId: string,
  target: IssuePriorityTarget,
  expectedActorId: string,
  idempotencyKey: string,
  input: IssuePriorityInput,
  signal: AbortSignal,
): Promise<IssuePriorityResult> {
  uuid(projectId);
  uuid(expectedActorId);
  uuid(idempotencyKey);
  const issue = {
    id: uuid(target.id),
    issue_number: decimal(target.issue_number),
    issue_key: target.issue_key,
  };
  if (
    typeof issue.issue_key !== 'string' ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(issue.issue_key) ||
    issue.issue_key.slice(issue.issue_key.indexOf('-') + 1) !==
      issue.issue_number
  ) {
    return invalid();
  }
  const selectedInput = fields(input, ['expected_version', 'priority']);
  const body = serializeIssuePriorityInput(selectedInput);
  const expectedVersion = decimal(selectedInput.expected_version);
  const selectedPriority = priority(selectedInput.priority);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const session = await getCurrentSession(deadline);
  deadline.throwIfAborted();
  if (session.user_id !== expectedActorId) {
    throw new IssuePriorityIdentityChanged();
  }

  let response: Response;
  try {
    response = await fetch(
      `/api/v1/projects/${projectId}/issues/${issue.issue_number}/priority`,
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
    throw new Error('Issue priority result could not be confirmed');
  }
  if (response.status !== 200) {
    return issueFailure(response, deadline);
  }
  const receipt = parseIssuePriorityReceipt(
    await issueJson(response, deadline, 4096),
    projectId,
    issue,
    { expected_version: expectedVersion, priority: selectedPriority },
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
