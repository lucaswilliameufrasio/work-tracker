import { serializeIssueCreationInput } from './issue-creation.ts';
import {
  fields,
  invalid,
  issueFailure,
  issueJson,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';

export type IssueTextInput = {
  title: string;
  description?: string;
  expected_version: string;
};
export type IssueTextTarget = { id: string; issue_number: string };
export type IssueTextReceipt = {
  id: string;
  project_id: string;
  issue_number: string;
  issue_key: string;
  result_version: string;
  changed: boolean;
};
export type IssueTextResult = { receipt: IssueTextReceipt; replayed: boolean };

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

export function serializeIssueTextInput(value: unknown): string {
  const item = fields(value, ['title', 'description', 'expected_version']);
  const version = decimal(item.expected_version);
  const text = serializeIssueCreationInput({
    title: item.title,
    ...(item.description === undefined
      ? {}
      : { description: item.description }),
  });
  const encoded = `${text.slice(0, -1)},"expected_version":${JSON.stringify(version)}}`;
  if (new TextEncoder().encode(encoded).length > 512 * 1024) {
    return invalid();
  }
  return encoded;
}

export function parseIssueTextReceipt(
  value: unknown,
  projectId: string,
  target: IssueTextTarget,
  expectedVersion: string,
): IssueTextReceipt {
  const item = fields(value, [
    'id',
    'project_id',
    'issue_number',
    'issue_key',
    'result_version',
    'changed',
  ]);
  const version = decimal(item.result_version);
  const expected = BigInt(decimal(expectedVersion));
  const number = decimal(item.issue_number);
  if (
    uuid(item.project_id) !== uuid(projectId) ||
    uuid(item.id) !== uuid(target.id) ||
    number !== decimal(target.issue_number) ||
    typeof item.changed !== 'boolean' ||
    BigInt(version) !== expected + (item.changed ? 1n : 0n) ||
    typeof item.issue_key !== 'string' ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(item.issue_key) ||
    item.issue_key.slice(item.issue_key.indexOf('-') + 1) !== number
  ) {
    return invalid();
  }
  return {
    id: uuid(item.id),
    project_id: uuid(item.project_id),
    issue_number: number,
    issue_key: item.issue_key,
    result_version: version,
    changed: item.changed,
  };
}

export async function replaceIssueText(
  projectId: string,
  target: IssueTextTarget,
  expectedActorId: string,
  idempotencyKey: string,
  input: IssueTextInput,
  signal: AbortSignal,
): Promise<IssueTextResult> {
  uuid(projectId);
  uuid(expectedActorId);
  uuid(idempotencyKey);
  const issue = {
    id: uuid(target.id),
    issue_number: decimal(target.issue_number),
  };
  const snapshot = fields(input, ['title', 'description', 'expected_version']);
  const encoded = serializeIssueTextInput(snapshot);
  const expectedVersion = decimal(snapshot.expected_version);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const session = await getCurrentSession(deadline);
  deadline.throwIfAborted();
  if (session.user_id !== expectedActorId) {
    throw new Error('Issue edit identity changed');
  }
  let response: Response;
  try {
    response = await fetch(
      `/api/v1/projects/${projectId}/issues/${issue.issue_number}/text`,
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
        body: encoded,
      },
    );
  } catch {
    deadline.throwIfAborted();
    throw new Error('Issue edit result could not be confirmed');
  }
  if (response.status !== 200) {
    return issueFailure(response, deadline);
  }
  const receipt = parseIssueTextReceipt(
    await issueJson(response, deadline, 4096),
    projectId,
    issue,
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
