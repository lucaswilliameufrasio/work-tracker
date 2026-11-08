import {
  fields,
  invalid,
  issueFailure,
  issueJson,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';

export type IssueCreationInput = {
  title: string;
  description?: string;
  assignee_user_id?: string;
};

export type IssueCreationReceipt = {
  id: string;
  project_id: string;
  issue_number: string;
  issue_key: string;
  created_version: '1';
};

export type IssueCreationResult = {
  receipt: IssueCreationReceipt;
  replayed: boolean;
};

function text(value: unknown): string {
  if (typeof value !== 'string') {
    return invalid();
  }
  for (const scalar of value) {
    const code = scalar.charCodeAt(0);
    if (scalar.length === 1 && code >= 0xd800 && code <= 0xdfff) {
      return invalid();
    }
  }
  return value;
}

export function serializeIssueCreationInput(input: unknown): string {
  const item = fields(input, ['title', 'description', 'assignee_user_id']);
  const title = text(item.title);
  const trimmed = title.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, '');
  if ([...trimmed].length < 1 || [...trimmed].length > 240) {
    return invalid();
  }
  const snapshot: IssueCreationInput = { title };
  if (item.description !== undefined) {
    snapshot.description = text(item.description);
    if (new TextEncoder().encode(snapshot.description).length > 65536) {
      return invalid();
    }
  }
  if (item.assignee_user_id !== undefined) {
    snapshot.assignee_user_id = uuid(item.assignee_user_id);
  }
  const encoded = JSON.stringify(snapshot);
  if (new TextEncoder().encode(encoded).length > 512 * 1024) {
    return invalid();
  }
  return encoded;
}

export function parseIssueCreationReceipt(
  value: unknown,
  projectId: string,
): IssueCreationReceipt {
  const item = fields(value, [
    'id',
    'project_id',
    'issue_number',
    'issue_key',
    'created_version',
  ]);
  const project = uuid(item.project_id);
  const number = item.issue_number;
  if (
    project !== uuid(projectId) ||
    typeof number !== 'string' ||
    !/^[1-9][0-9]{0,18}$/.test(number) ||
    BigInt(number) > 9223372036854775807n ||
    typeof item.issue_key !== 'string' ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(item.issue_key) ||
    item.issue_key.slice(item.issue_key.indexOf('-') + 1) !== number ||
    item.created_version !== '1'
  ) {
    return invalid();
  }
  return {
    id: uuid(item.id),
    project_id: project,
    issue_number: number,
    issue_key: item.issue_key,
    created_version: '1',
  };
}

// The caller preserves the original key/payload and expected actor after an
// uncertain result. This function never generates a key or retries a mutation.
export async function createIssue(
  projectId: string,
  expectedActorId: string,
  idempotencyKey: string,
  input: IssueCreationInput,
  signal: AbortSignal,
): Promise<IssueCreationResult> {
  uuid(projectId);
  uuid(expectedActorId);
  uuid(idempotencyKey);
  const encoded = serializeIssueCreationInput(input);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const session = await getCurrentSession(deadline);
  deadline.throwIfAborted();
  if (session.user_id !== expectedActorId) {
    throw new Error('Issue creation identity changed');
  }
  let response: Response;
  try {
    response = await fetch(`/api/v1/projects/${projectId}/issues`, {
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
      body: encoded,
    });
  } catch {
    deadline.throwIfAborted();
    throw new Error('Issue creation result could not be confirmed');
  }
  if (response.status !== 201) {
    return issueFailure(response, deadline);
  }
  const receipt = parseIssueCreationReceipt(
    await issueJson(response, deadline, 4096),
    projectId,
  );
  const replayed = response.headers.get('idempotency-replayed');
  if (
    (replayed !== 'true' && replayed !== 'false') ||
    response.headers.get('location') !==
      `/api/v1/projects/${projectId}/issues/${receipt.issue_number}`
  ) {
    return invalid();
  }
  return { receipt, replayed: replayed === 'true' };
}
