import {
  fields,
  invalid,
  issueFailure,
  issueJson,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';

export type IssueMoveInput = {
  expected_version: string;
  target_status_id: string;
  previous_issue_id?: string;
  next_issue_id?: string;
};
export type IssueMoveTarget = { id: string; issue_number: string };
export type IssueMoveReceipt = {
  id: string;
  project_id: string;
  issue_number: string;
  issue_key: string;
  status_id: string;
  result_version: string;
  changed: boolean;
};
export type IssueMoveResult = {
  receipt: IssueMoveReceipt;
  replayed: boolean;
};

export class IssueMoveIdentityChanged extends Error {
  constructor() {
    super('Issue move identity changed');
    this.name = 'IssueMoveIdentityChanged';
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

export function serializeIssueMoveInput(value: unknown): string {
  const item = fields(value, [
    'expected_version',
    'target_status_id',
    'previous_issue_id',
    'next_issue_id',
  ]);
  const expectedVersion = decimal(item.expected_version);
  const targetStatusId = uuid(item.target_status_id);
  const previousIssueId =
    item.previous_issue_id === undefined
      ? undefined
      : uuid(item.previous_issue_id);
  const nextIssueId =
    item.next_issue_id === undefined ? undefined : uuid(item.next_issue_id);
  if (previousIssueId !== undefined && previousIssueId === nextIssueId) {
    return invalid();
  }
  const encoded = JSON.stringify({
    expected_version: expectedVersion,
    target_status_id: targetStatusId,
    ...(previousIssueId === undefined
      ? {}
      : { previous_issue_id: previousIssueId }),
    ...(nextIssueId === undefined ? {} : { next_issue_id: nextIssueId }),
  });
  if (new TextEncoder().encode(encoded).length > 512 * 1024) {
    return invalid();
  }
  return encoded;
}

export function parseIssueMoveReceipt(
  value: unknown,
  projectId: string,
  target: IssueMoveTarget,
  targetStatusId: string,
  expectedVersion: string,
): IssueMoveReceipt {
  const item = fields(value, [
    'id',
    'project_id',
    'issue_number',
    'issue_key',
    'status_id',
    'result_version',
    'changed',
  ]);
  const resultVersion = decimal(item.result_version);
  const expected = BigInt(decimal(expectedVersion));
  const issueNumber = decimal(item.issue_number);
  if (
    uuid(item.project_id) !== uuid(projectId) ||
    uuid(item.id) !== uuid(target.id) ||
    issueNumber !== decimal(target.issue_number) ||
    uuid(item.status_id) !== uuid(targetStatusId) ||
    typeof item.changed !== 'boolean' ||
    BigInt(resultVersion) !== expected + (item.changed ? 1n : 0n) ||
    typeof item.issue_key !== 'string' ||
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
    status_id: uuid(item.status_id),
    result_version: resultVersion,
    changed: item.changed,
  };
}

export async function moveIssue(
  projectId: string,
  target: IssueMoveTarget,
  expectedActorId: string,
  idempotencyKey: string,
  input: IssueMoveInput,
  signal: AbortSignal,
): Promise<IssueMoveResult> {
  uuid(projectId);
  uuid(expectedActorId);
  uuid(idempotencyKey);
  const issue = {
    id: uuid(target.id),
    issue_number: decimal(target.issue_number),
  };
  const selectedInput = fields(input, [
    'expected_version',
    'target_status_id',
    'previous_issue_id',
    'next_issue_id',
  ]);
  const body = serializeIssueMoveInput(selectedInput);
  const expectedVersion = decimal(selectedInput.expected_version);
  const targetStatusId = uuid(selectedInput.target_status_id);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const session = await getCurrentSession(deadline);
  deadline.throwIfAborted();
  if (session.user_id !== expectedActorId) {
    throw new IssueMoveIdentityChanged();
  }

  let response: Response;
  try {
    response = await fetch(
      `/api/v1/projects/${projectId}/issues/${issue.issue_number}/move`,
      {
        method: 'POST',
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
    throw new Error('Issue move result could not be confirmed');
  }
  if (response.status !== 200) {
    return issueFailure(response, deadline);
  }
  const receipt = parseIssueMoveReceipt(
    await issueJson(response, deadline, 4096),
    projectId,
    issue,
    targetStatusId,
    expectedVersion,
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
