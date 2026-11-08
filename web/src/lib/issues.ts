import { fields, invalid, readIssueJson, uuid } from './issue-transport.ts';
export { IssueRequestError } from './issue-transport.ts';

export type IssueSummary = {
  id: string;
  project_id: string;
  issue_number: string;
  issue_key: string;
  issue_type_id: string;
  status_id: string;
  title: string;
  assignee_user_id?: string;
  creator_user_id: string;
  priority: 'none' | 'low' | 'medium' | 'high' | 'urgent';
  due_date?: string;
  is_overdue: boolean;
  version: string;
  created_at: string;
  updated_at: string;
};
export type IssueDetail = IssueSummary & {
  description?: string;
  completed_at?: string;
  trashed_at?: string;
};
export type IssuePriority = IssueSummary['priority'];
export type IssueHistoryEvent = {
  id: string;
  project_id: string;
  issue_id: string;
  actor_user_id: string;
  event_kind:
    | 'issue.created'
    | 'issue.title_updated'
    | 'issue.description_updated'
    | 'issue.text_updated'
    | 'issue.moved'
    | 'issue.field_updated';
  change?:
    | { field: 'priority'; before: IssuePriority; after: IssuePriority }
    | { field: 'due_date'; before: string | null; after: string | null }
    | {
        field: 'assignee_user_id';
        before: string | null;
        after: string | null;
      };
  occurred_at: string;
};
export type IssuePage<T> = {
  data: T[];
  has_more: boolean;
  next_cursor?: string;
};
export type PageOptions = { cursor?: string; page_size?: number };

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
];
const detailFields = [
  ...summaryFields,
  'description',
  'completed_at',
  'trashed_at',
];

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

function date(value: unknown): string {
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

function timestamp(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z$/.test(value) ||
    !Number.isFinite(Date.parse(value)) ||
    new Date(value).toISOString().slice(0, 19) !== value.slice(0, 19)
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

export function validateIssueSearchPriority(value: unknown): IssuePriority {
  return priority(value);
}

export function validateIssueSearchDueDate(value: unknown): string {
  return date(value);
}

export function validateIssueSearchIssueType(value: unknown): string {
  return uuid(value);
}

export function validateIssueSearchStatus(value: unknown): string {
  return uuid(value);
}

export function validateIssueSearchAssignee(value: unknown): string {
  return uuid(value);
}

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

function summary(
  item: Record<string, unknown>,
  projectId: string,
): IssueSummary {
  const project = uuid(item.project_id);
  const number = decimal(item.issue_number);
  const title = text(item.title);
  if (
    project !== uuid(projectId) ||
    typeof item.issue_key !== 'string' ||
    !/^[A-Z]{2,6}-[1-9][0-9]*$/.test(item.issue_key) ||
    item.issue_key.slice(item.issue_key.indexOf('-') + 1) !== number ||
    [...title].length < 1 ||
    [...title].length > 240 ||
    title.trim().length === 0
  ) {
    return invalid();
  }
  const issuePriority = priority(item.priority);
  if (typeof item.is_overdue !== 'boolean') {
    return invalid();
  }
  const result: IssueSummary = {
    id: uuid(item.id),
    project_id: project,
    issue_number: number,
    issue_key: item.issue_key,
    issue_type_id: uuid(item.issue_type_id),
    status_id: uuid(item.status_id),
    title,
    creator_user_id: uuid(item.creator_user_id),
    priority: issuePriority,
    is_overdue: item.is_overdue,
    version: decimal(item.version),
    created_at: timestamp(item.created_at),
    updated_at: timestamp(item.updated_at),
  };
  if ('assignee_user_id' in item) {
    result.assignee_user_id = uuid(item.assignee_user_id);
  }
  if ('due_date' in item) {
    result.due_date = date(item.due_date);
  }
  return result;
}

export function parseIssueSummary(
  value: unknown,
  projectId: string,
): IssueSummary {
  return summary(fields(value, summaryFields), projectId);
}

export function parseIssueDetail(
  value: unknown,
  projectId: string,
  number: string,
): IssueDetail {
  const item = fields(value, detailFields);
  const result: IssueDetail = summary(item, projectId);
  if (result.issue_number !== decimal(number)) {
    return invalid();
  }
  if ('description' in item) {
    const description = text(item.description);
    if (new TextEncoder().encode(description).byteLength > 65536) {
      return invalid();
    }
    result.description = description;
  }
  if ('completed_at' in item) {
    result.completed_at = timestamp(item.completed_at);
  }
  if ('trashed_at' in item) {
    result.trashed_at = timestamp(item.trashed_at);
  }
  return result;
}

function size(value = 50): number {
  if (!Number.isInteger(value) || value < 1 || value > 100) {
    return invalid();
  }
  return value;
}

function page(
  value: unknown,
  pageSize: number,
): Record<string, unknown> & { data: unknown[]; has_more: boolean } {
  const item = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(item.data) ||
    item.data.length > size(pageSize) ||
    typeof item.has_more !== 'boolean' ||
    (item.has_more && item.data.length !== pageSize) ||
    (!item.has_more && 'next_cursor' in item)
  ) {
    return invalid();
  }
  return { ...item, data: item.data, has_more: item.has_more };
}

export function parseIssuePage(
  value: unknown,
  projectId: string,
  pageSize: number,
  after?: string,
): IssuePage<IssueSummary> {
  uuid(projectId);
  const item = page(value, pageSize);
  let previous = after === undefined ? 0n : BigInt(decimal(after));
  const data: IssueSummary[] = [];
  const ids = new Set<string>();
  for (const raw of item.data) {
    const issue = summary(fields(raw, summaryFields), projectId);
    const number = BigInt(issue.issue_number);
    if (number <= previous || ids.has(issue.id)) {
      return invalid();
    }
    previous = number;
    ids.add(issue.id);
    data.push(issue);
  }
  if (item.has_more) {
    const cursor = decimal(item.next_cursor);
    if (BigInt(cursor) !== previous) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: cursor };
  }
  return { data, has_more: false };
}

type SearchBoundary = { issue_number: string; issue_id: string };

function searchText(value: unknown): string {
  const query = text(value);
  const terms = query
    .split(/[\u0020\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+/u)
    .filter((term) => term.length > 0);
  if (
    new TextEncoder().encode(query).byteLength > 512 ||
    [...query].length > 256 ||
    /\p{Cc}/u.test(query) ||
    terms.length < 1 ||
    terms.length > 16
  ) {
    return invalid();
  }
  return query;
}

export function validateIssueSearchText(value: unknown): string {
  return searchText(value);
}

function searchBoundary(value: unknown): SearchBoundary {
  if (
    typeof value !== 'string' ||
    value.length < 51 ||
    value.length > 75 ||
    !/^[A-Za-z0-9_-]+$/.test(value)
  ) {
    return invalid();
  }
  let raw: string;
  try {
    raw = atob(value.replaceAll('-', '+').replaceAll('_', '/'));
  } catch {
    return invalid();
  }
  if (
    btoa(raw).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '') !==
      value ||
    raw.length < 38 ||
    raw.length > 56
  ) {
    return invalid();
  }
  const separator = raw.indexOf('/');
  if (separator < 0 || raw.indexOf('/', separator + 1) >= 0) {
    return invalid();
  }
  const issueNumber = decimal(raw.slice(0, separator));
  const issueId = uuid(raw.slice(separator + 1));
  return { issue_number: issueNumber, issue_id: issueId };
}

export function validateIssueSearchCursor(value: unknown): string {
  if (typeof value !== 'string') {
    return invalid();
  }
  searchBoundary(value);
  return value;
}

function searchBoundaryAfter(
  left: SearchBoundary,
  right: SearchBoundary,
): boolean {
  const leftNumber = BigInt(left.issue_number);
  const rightNumber = BigInt(right.issue_number);
  return (
    leftNumber > rightNumber ||
    (leftNumber === rightNumber && left.issue_id > right.issue_id)
  );
}

export function parseIssueSearchPage(
  value: unknown,
  projectId: string,
  query: string | undefined,
  pageSize: number,
  after?: string,
  priorityFilter?: IssuePriority,
  dueDateFilter?: string,
  issueTypeFilter?: string,
  statusFilter?: string,
  assigneeFilter?: string,
): IssuePage<IssueSummary> {
  uuid(projectId);
  if (query !== undefined) {
    searchText(query);
  }
  if (priorityFilter !== undefined) {
    priority(priorityFilter);
  }
  if (dueDateFilter !== undefined) {
    date(dueDateFilter);
  }
  if (issueTypeFilter !== undefined) {
    uuid(issueTypeFilter);
  }
  if (statusFilter !== undefined) {
    uuid(statusFilter);
  }
  if (assigneeFilter !== undefined) {
    uuid(assigneeFilter);
  }
  if (
    query === undefined &&
    priorityFilter === undefined &&
    dueDateFilter === undefined &&
    issueTypeFilter === undefined &&
    statusFilter === undefined &&
    assigneeFilter === undefined
  ) {
    return invalid();
  }
  const item = page(value, pageSize);
  let previous =
    after === undefined
      ? { issue_number: '0', issue_id: '' }
      : searchBoundary(after);
  const data: IssueSummary[] = [];
  const ids = new Set<string>();
  for (const raw of item.data) {
    const issue = summary(fields(raw, summaryFields), projectId);
    if (priorityFilter !== undefined && issue.priority !== priorityFilter) {
      return invalid();
    }
    if (dueDateFilter !== undefined && issue.due_date !== dueDateFilter) {
      return invalid();
    }
    if (
      issueTypeFilter !== undefined &&
      issue.issue_type_id !== issueTypeFilter
    ) {
      return invalid();
    }
    if (statusFilter !== undefined && issue.status_id !== statusFilter) {
      return invalid();
    }
    if (
      assigneeFilter !== undefined &&
      issue.assignee_user_id !== assigneeFilter
    ) {
      return invalid();
    }
    const boundary = {
      issue_number: issue.issue_number,
      issue_id: issue.id,
    };
    if (!searchBoundaryAfter(boundary, previous) || ids.has(issue.id)) {
      return invalid();
    }
    previous = boundary;
    ids.add(issue.id);
    data.push(issue);
  }
  if (item.has_more) {
    const nextCursor = validateIssueSearchCursor(item.next_cursor);
    const cursor = searchBoundary(nextCursor);
    if (
      cursor.issue_number !== previous.issue_number ||
      cursor.issue_id !== previous.issue_id
    ) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: nextCursor };
  }
  return { data, has_more: false };
}

function historyBoundary(value: unknown): string {
  if (typeof value !== 'string' || !/^[A-Za-z0-9_-]{90}$/.test(value)) {
    return invalid();
  }
  const raw = atob(value.replaceAll('-', '+').replaceAll('_', '/'));
  if (
    btoa(raw).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '') !==
      value ||
    raw.length !== 67 ||
    raw[30] !== '/'
  ) {
    return invalid();
  }
  timestamp(raw.slice(0, 30));
  uuid(raw.slice(31));
  return raw;
}

export function parseIssueHistoryPage(
  value: unknown,
  projectId: string,
  pageSize: number,
  after?: string,
): IssuePage<IssueHistoryEvent> {
  uuid(projectId);
  const item = page(value, pageSize);
  let previous = after === undefined ? '' : historyBoundary(after);
  let issueId: string | undefined;
  const data: IssueHistoryEvent[] = [];
  const ids = new Set<string>();
  for (const raw of item.data) {
    const event = fields(raw, [
      'id',
      'project_id',
      'issue_id',
      'actor_user_id',
      'event_kind',
      'change',
      'occurred_at',
    ]);
    const id = uuid(event.id);
    const issue = uuid(event.issue_id);
    const occurred = timestamp(event.occurred_at);
    const boundary = `${occurred}/${id}`;
    const kind = event.event_kind;
    let change: IssueHistoryEvent['change'];
    if (kind === 'issue.field_updated') {
      const rawChange = fields(event.change, ['field', 'before', 'after']);
      if (rawChange.field === 'priority') {
        const before = priority(rawChange.before);
        const after = priority(rawChange.after);
        if (before === after) {
          return invalid();
        }
        change = { field: 'priority', before, after };
      } else if (rawChange.field === 'due_date') {
        const before =
          rawChange.before === null ? null : date(rawChange.before);
        const after = rawChange.after === null ? null : date(rawChange.after);
        if (before === after) {
          return invalid();
        }
        change = { field: 'due_date', before, after };
      } else if (rawChange.field === 'assignee_user_id') {
        const before =
          rawChange.before === null ? null : uuid(rawChange.before);
        const after = rawChange.after === null ? null : uuid(rawChange.after);
        if (before === after) {
          return invalid();
        }
        change = { field: 'assignee_user_id', before, after };
      } else {
        return invalid();
      }
    } else if ('change' in event) {
      return invalid();
    }
    if (
      uuid(event.project_id) !== projectId ||
      (issueId !== undefined && issue !== issueId) ||
      (kind !== 'issue.created' &&
        kind !== 'issue.title_updated' &&
        kind !== 'issue.description_updated' &&
        kind !== 'issue.text_updated' &&
        kind !== 'issue.moved' &&
        kind !== 'issue.field_updated') ||
      boundary <= previous ||
      ids.has(id)
    ) {
      return invalid();
    }
    issueId = issue;
    previous = boundary;
    ids.add(id);
    data.push({
      id,
      project_id: projectId,
      issue_id: issue,
      actor_user_id: uuid(event.actor_user_id),
      event_kind: kind,
      ...(change === undefined ? {} : { change }),
      occurred_at: occurred,
    });
  }
  if (item.has_more) {
    const cursor = item.next_cursor;
    if (typeof cursor !== 'string' || historyBoundary(cursor) !== previous) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: cursor };
  }
  return { data, has_more: false };
}

function projectPath(projectId: string): string {
  return `/api/v1/projects/${uuid(projectId)}/issues`;
}

function query(
  options: PageOptions,
  history: boolean,
): { encoded: URLSearchParams; pageSize: number } {
  const pageSize = size(options.page_size);
  const encoded = new URLSearchParams({ page_size: String(pageSize) });
  if (options.cursor !== undefined) {
    if (history) {
      historyBoundary(options.cursor);
    } else {
      decimal(options.cursor);
    }
    encoded.set('cursor', options.cursor);
  }
  return { encoded, pageSize };
}

export async function getIssuePage(
  projectId: string,
  options: PageOptions,
  signal: AbortSignal,
): Promise<IssuePage<IssueSummary>> {
  const path = projectPath(projectId);
  const { encoded, pageSize } = query(options, false);
  return parseIssuePage(
    await readIssueJson(`${path}?${encoded}`, signal),
    projectId,
    pageSize,
    options.cursor,
  );
}

export async function getIssueSearchPage(
  projectId: string,
  queryText: string | undefined,
  options: PageOptions,
  signal: AbortSignal,
  priorityFilter?: IssuePriority,
  dueDateFilter?: string,
  issueTypeFilter?: string,
  statusFilter?: string,
  assigneeFilter?: string,
): Promise<IssuePage<IssueSummary>> {
  uuid(projectId);
  const search = queryText === undefined ? undefined : searchText(queryText);
  const selectedPriority =
    priorityFilter === undefined ? undefined : priority(priorityFilter);
  const selectedDueDate =
    dueDateFilter === undefined ? undefined : date(dueDateFilter);
  const selectedIssueType =
    issueTypeFilter === undefined ? undefined : uuid(issueTypeFilter);
  const selectedStatus =
    statusFilter === undefined ? undefined : uuid(statusFilter);
  const selectedAssignee =
    assigneeFilter === undefined ? undefined : uuid(assigneeFilter);
  if (
    search === undefined &&
    selectedPriority === undefined &&
    selectedDueDate === undefined &&
    selectedIssueType === undefined &&
    selectedStatus === undefined &&
    selectedAssignee === undefined
  ) {
    return invalid();
  }
  const pageSize = size(options.page_size);
  const encoded = new URLSearchParams();
  if (search !== undefined) {
    encoded.set('q', search);
  }
  if (selectedPriority !== undefined) {
    encoded.set('priority', selectedPriority);
  }
  if (selectedDueDate !== undefined) {
    encoded.set('due_date', selectedDueDate);
  }
  if (selectedIssueType !== undefined) {
    encoded.set('issue_type_id', selectedIssueType);
  }
  if (selectedStatus !== undefined) {
    encoded.set('status_id', selectedStatus);
  }
  if (selectedAssignee !== undefined) {
    encoded.set('assignee_user_id', selectedAssignee);
  }
  encoded.set('page_size', String(pageSize));
  if (options.cursor !== undefined) {
    searchBoundary(options.cursor);
    encoded.set('cursor', options.cursor);
  }
  const path = `${projectPath(projectId)}/search`;
  return parseIssueSearchPage(
    await readIssueJson(`${path}?${encoded}`, signal),
    projectId,
    search,
    pageSize,
    options.cursor,
    selectedPriority,
    selectedDueDate,
    selectedIssueType,
    selectedStatus,
    selectedAssignee,
  );
}

export async function getIssueDetail(
  projectId: string,
  number: string,
  signal: AbortSignal,
): Promise<IssueDetail> {
  const path = `${projectPath(projectId)}/${decimal(number)}`;
  return parseIssueDetail(await readIssueJson(path, signal), projectId, number);
}

export async function getIssueHistoryPage(
  projectId: string,
  number: string,
  options: PageOptions,
  signal: AbortSignal,
): Promise<IssuePage<IssueHistoryEvent>> {
  const path = `${projectPath(projectId)}/${decimal(number)}/history`;
  const { encoded, pageSize } = query(options, true);
  return parseIssueHistoryPage(
    await readIssueJson(`${path}?${encoded}`, signal),
    projectId,
    pageSize,
    options.cursor,
  );
}
