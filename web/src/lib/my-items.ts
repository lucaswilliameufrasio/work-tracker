import { fields, invalid, issueJson, uuid } from './issue-transport.ts';
import type { IssuePage, IssueSummary } from './issues.ts';

const summaryFields = [
  'id',
  'project_id',
  'issue_number',
  'issue_key',
  'issue_type_id',
  'status_id',
  'title',
  'assignee_user_id',
  'creator_user_id',
  'priority',
  'due_date',
  'is_overdue',
  'version',
  'created_at',
  'updated_at',
] as const;

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

function timestamp(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z$/.test(value)
  ) {
    return invalid();
  }
  return value;
}

function summary(item: Record<string, unknown>): IssueSummary {
  const projectId = uuid(item.project_id);
  const issueNumber = decimal(item.issue_number);
  const issueKey = item.issue_key;
  if (
    typeof issueKey !== 'string' ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(issueKey) ||
    issueKey.slice(issueKey.indexOf('-') + 1) !== issueNumber ||
    typeof item.title !== 'string' ||
    item.title.trim().length === 0 ||
    [...item.title].length > 240 ||
    typeof item.is_overdue !== 'boolean' ||
    (item.priority !== 'none' &&
      item.priority !== 'low' &&
      item.priority !== 'medium' &&
      item.priority !== 'high' &&
      item.priority !== 'urgent')
  ) {
    return invalid();
  }
  const dueDate = item.due_date;
  if (
    dueDate !== undefined &&
    (typeof dueDate !== 'string' || !/^\d{4}-\d{2}-\d{2}$/.test(dueDate))
  ) {
    return invalid();
  }
  const assignee = item.assignee_user_id;
  if (assignee !== undefined && typeof assignee !== 'string') {
    return invalid();
  }
  return {
    id: uuid(item.id),
    project_id: projectId,
    issue_number: issueNumber,
    issue_key: issueKey,
    issue_type_id: uuid(item.issue_type_id),
    status_id: uuid(item.status_id),
    title: item.title,
    ...(assignee === undefined ? {} : { assignee_user_id: uuid(assignee) }),
    creator_user_id: uuid(item.creator_user_id),
    priority: item.priority,
    ...(dueDate === undefined ? {} : { due_date: dueDate }),
    is_overdue: item.is_overdue,
    version: decimal(item.version),
    created_at: timestamp(item.created_at),
    updated_at: timestamp(item.updated_at),
  };
}

export function parseAssignedIssuePage(
  value: unknown,
  pageSize: number,
): IssuePage<IssueSummary> {
  const item = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(item.data) ||
    item.data.length > pageSize ||
    typeof item.has_more !== 'boolean' ||
    (item.has_more && item.data.length !== pageSize) ||
    (!item.has_more && 'next_cursor' in item) ||
    (item.has_more &&
      (typeof item.next_cursor !== 'string' || item.next_cursor.length === 0))
  ) {
    return invalid();
  }
  const data = item.data.map((entry) => summary(fields(entry, summaryFields)));
  return {
    data,
    has_more: item.has_more,
    ...(item.has_more ? { next_cursor: item.next_cursor as string } : {}),
  };
}

export async function listAssignedIssues(
  signal: AbortSignal,
  cursor?: string,
): Promise<IssuePage<IssueSummary>> {
  const query = new URLSearchParams({ page_size: '50' });
  if (cursor !== undefined) {
    query.set('cursor', cursor);
  }
  const response = await fetch(`/api/v1/me/issues?${query.toString()}`, {
    credentials: 'same-origin',
    cache: 'no-store',
    signal,
  });
  if (!response.ok) {
    try {
      await response.body?.cancel();
    } catch {
      signal.throwIfAborted();
    }
    throw new Error('Assigned issues request failed');
  }
  const payload = await issueJson(response, signal, 1_048_576);
  return parseAssignedIssuePage(payload, 50);
}
