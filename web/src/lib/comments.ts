import { getCurrentSession } from './sessions';

export interface IssueComment {
  id: string;
  author_user_id: string;
  author_display_name: string;
  body: string;
  version: string;
  can_edit: boolean;
  can_delete: boolean;
  created_at: string;
  updated_at: string;
}

export interface IssueCommentEvent {
  id: string;
  comment_id: string;
  actor_user_id: string;
  event_kind: 'comment.created' | 'comment.edited' | 'comment.removed';
  occurred_at: string;
}

export interface IssueCommentPage {
  comments: IssueComment[];
  events: IssueCommentEvent[];
  has_more: boolean;
  next_cursor?: string;
}

export class CommentRequestError extends Error {
  readonly status: number;

  constructor(status: number) {
    super('Comment request failed');
    this.name = 'CommentRequestError';
    this.status = status;
  }
}

const commentKeys = [
  'id',
  'author_user_id',
  'author_display_name',
  'body',
  'version',
  'can_edit',
  'can_delete',
  'created_at',
  'updated_at',
] as const;

export function parseCommentPage(value: unknown): IssueCommentPage {
  if (!isRecord(value)) {
    throw new Error('Invalid comment API response');
  }
  const keys = Object.keys(value);
  for (const key of keys) {
    if (
      key !== 'comments' &&
      key !== 'events' &&
      key !== 'has_more' &&
      key !== 'next_cursor'
    ) {
      throw new Error('Invalid comment API response');
    }
  }
  if (
    !Array.isArray(value.comments) ||
    !Array.isArray(value.events) ||
    typeof value.has_more !== 'boolean'
  ) {
    throw new Error('Invalid comment API response');
  }
  if (
    value.next_cursor !== undefined &&
    typeof value.next_cursor !== 'string'
  ) {
    throw new Error('Invalid comment API response');
  }
  const comments = value.comments.map(parseComment);
  const events = value.events.map(parseEvent);
  const page: IssueCommentPage = {
    comments,
    events,
    has_more: value.has_more,
  };
  if (typeof value.next_cursor === 'string') {
    page.next_cursor = value.next_cursor;
  }
  return page;
}

export async function listComments(
  projectId: string,
  issueNumber: string,
  signal: AbortSignal,
): Promise<IssueCommentPage> {
  const response = await fetch(
    `/api/v1/projects/${projectId}/issues/${issueNumber}/comments`,
    {
      method: 'GET',
      mode: 'same-origin',
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      signal,
      headers: { Accept: 'application/json' },
    },
  );
  if (response.status !== 200) {
    throw new CommentRequestError(response.status);
  }
  return parseCommentPage(await response.json());
}

export async function createComment(
  projectId: string,
  issueNumber: string,
  body: string,
  signal: AbortSignal,
): Promise<void> {
  await sendComment(
    `/api/v1/projects/${projectId}/issues/${issueNumber}/comments`,
    'POST',
    { body },
    201,
    signal,
  );
}

export async function editComment(
  projectId: string,
  issueNumber: string,
  commentId: string,
  body: string,
  expectedVersion: string,
  signal: AbortSignal,
): Promise<void> {
  await sendComment(
    `/api/v1/projects/${projectId}/issues/${issueNumber}/comments/${commentId}`,
    'PUT',
    { body, expected_version: expectedVersion },
    200,
    signal,
  );
}

export async function removeComment(
  projectId: string,
  issueNumber: string,
  commentId: string,
  signal: AbortSignal,
): Promise<void> {
  await sendComment(
    `/api/v1/projects/${projectId}/issues/${issueNumber}/comments/${commentId}`,
    'DELETE',
    undefined,
    204,
    signal,
  );
}

async function sendComment(
  path: string,
  method: 'POST' | 'PUT' | 'DELETE',
  payload: { body: string; expected_version?: string } | undefined,
  expectedStatus: number,
  signal: AbortSignal,
): Promise<void> {
  const session = await getCurrentSession(signal);
  const headers: Record<string, string> = {
    Accept: 'application/json',
    'x-csrf-token': session.csrf_token,
    'Idempotency-Key': crypto.randomUUID(),
  };
  let body: string | undefined;
  if (payload !== undefined) {
    headers['Content-Type'] = 'application/json';
    body = JSON.stringify(payload);
  }
  const response = await fetch(path, {
    method,
    mode: 'same-origin',
    credentials: 'same-origin',
    cache: 'no-store',
    redirect: 'error',
    signal,
    headers,
    body,
  });
  if (response.status !== expectedStatus) {
    throw new CommentRequestError(response.status);
  }
}

function parseComment(value: unknown): IssueComment {
  if (!isRecord(value) || Object.keys(value).length !== commentKeys.length) {
    throw new Error('Invalid comment API response');
  }
  for (const key of commentKeys) {
    if (!(key in value)) {
      throw new Error('Invalid comment API response');
    }
  }
  if (
    typeof value.id !== 'string' ||
    typeof value.author_user_id !== 'string' ||
    typeof value.author_display_name !== 'string' ||
    typeof value.body !== 'string' ||
    typeof value.version !== 'string' ||
    typeof value.can_edit !== 'boolean' ||
    typeof value.can_delete !== 'boolean' ||
    typeof value.created_at !== 'string' ||
    typeof value.updated_at !== 'string'
  ) {
    throw new Error('Invalid comment API response');
  }
  return {
    id: value.id,
    author_user_id: value.author_user_id,
    author_display_name: value.author_display_name,
    body: value.body,
    version: value.version,
    can_edit: value.can_edit,
    can_delete: value.can_delete,
    created_at: value.created_at,
    updated_at: value.updated_at,
  };
}

function parseEvent(value: unknown): IssueCommentEvent {
  if (!isRecord(value)) {
    throw new Error('Invalid comment API response');
  }
  if (
    typeof value.id !== 'string' ||
    typeof value.comment_id !== 'string' ||
    typeof value.actor_user_id !== 'string' ||
    typeof value.occurred_at !== 'string' ||
    (value.event_kind !== 'comment.created' &&
      value.event_kind !== 'comment.edited' &&
      value.event_kind !== 'comment.removed')
  ) {
    throw new Error('Invalid comment API response');
  }
  return {
    id: value.id,
    comment_id: value.comment_id,
    actor_user_id: value.actor_user_id,
    event_kind: value.event_kind,
    occurred_at: value.occurred_at,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
