import {
  fields,
  invalid,
  issueFailure,
  issueJson,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';

export type IssueAssigneeValue = string | null;
export type IssueAssigneeInput = {
  expected_version: string;
  assignee_user_id: IssueAssigneeValue;
};
export type IssueAssigneeTarget = {
  id: string;
  issue_number: string;
  issue_key: string;
};
export type IssueAssigneeReceipt = IssueAssigneeTarget & {
  project_id: string;
  assignee_user_id: IssueAssigneeValue;
  result_version: string;
  changed: boolean;
};
export type IssueAssigneeResult = {
  receipt: IssueAssigneeReceipt;
  replayed: boolean;
};

export class IssueAssigneeIdentityChanged extends Error {
  constructor() {
    super('Issue assignee identity changed');
    this.name = 'IssueAssigneeIdentityChanged';
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

function assignee(value: unknown): IssueAssigneeValue {
  if (value === null) {
    return null;
  }
  return uuid(value);
}

function target(value: IssueAssigneeTarget): IssueAssigneeTarget {
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

export function serializeIssueAssigneeInput(value: unknown): string {
  const item = fields(value, ['expected_version', 'assignee_user_id']);
  const encoded = JSON.stringify({
    expected_version: decimal(item.expected_version),
    assignee_user_id: assignee(item.assignee_user_id),
  });
  if (new TextEncoder().encode(encoded).length > 512 * 1024) {
    return invalid();
  }
  return encoded;
}

export function parseIssueAssigneeReceipt(
  value: unknown,
  projectId: string,
  selectedTarget: IssueAssigneeTarget,
  input: IssueAssigneeInput,
): IssueAssigneeReceipt {
  const item = fields(value, [
    'id',
    'project_id',
    'issue_number',
    'issue_key',
    'assignee_user_id',
    'result_version',
    'changed',
  ]);
  const expectedVersion = BigInt(decimal(input.expected_version));
  const selectedIssue = target(selectedTarget);
  const issueNumber = decimal(item.issue_number);
  const resultVersion = decimal(item.result_version);
  const resultAssignee = assignee(item.assignee_user_id);
  const selectedAssignee = assignee(input.assignee_user_id);
  if (
    uuid(item.project_id) !== uuid(projectId) ||
    uuid(item.id) !== selectedIssue.id ||
    issueNumber !== selectedIssue.issue_number ||
    resultAssignee !== selectedAssignee ||
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
    assignee_user_id: resultAssignee,
    result_version: resultVersion,
    changed: item.changed,
  };
}

export async function setIssueAssignee(
  projectId: string,
  selectedTarget: IssueAssigneeTarget,
  expectedActorId: string,
  idempotencyKey: string,
  input: IssueAssigneeInput,
  signal: AbortSignal,
): Promise<IssueAssigneeResult> {
  uuid(projectId);
  uuid(expectedActorId);
  uuid(idempotencyKey);
  const issue = target(selectedTarget);
  const selectedInput = fields(input, ['expected_version', 'assignee_user_id']);
  const body = serializeIssueAssigneeInput(selectedInput);
  const expectedVersion = decimal(selectedInput.expected_version);
  const assigneeUserId = assignee(selectedInput.assignee_user_id);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const session = await getCurrentSession(deadline);
  deadline.throwIfAborted();
  if (session.user_id !== expectedActorId) {
    throw new IssueAssigneeIdentityChanged();
  }

  let response: Response;
  try {
    response = await fetch(
      `/api/v1/projects/${projectId}/issues/${issue.issue_number}/assignee`,
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
    throw new Error('Issue assignee result could not be confirmed');
  }
  if (response.status !== 200) {
    return issueFailure(response, deadline);
  }
  const receipt = parseIssueAssigneeReceipt(
    await issueJson(response, deadline, 4096),
    projectId,
    issue,
    {
      expected_version: expectedVersion,
      assignee_user_id: assigneeUserId,
    },
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
