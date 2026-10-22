import { fields, invalid, readIssueJson, uuid } from './issue-transport.ts';
import type { IssuePage } from './issues.ts';

type CatalogCommon = {
  id: string;
  project_id: string;
  name: string;
  system_key?: string;
  archived_at?: string;
};
export type ProjectStatus = CatalogCommon & {
  category: 'backlog' | 'active' | 'done' | 'canceled';
  position: string;
  board_visible: boolean;
};
export type ProjectIssueType = CatalogCommon & {
  icon_key?: string;
  color_key?: string;
};
export type CatalogOptions = {
  cursor?: string;
  page_size?: number;
  include_archived?: boolean;
};

const commonFields = ['id', 'project_id', 'name', 'system_key', 'archived_at'];

function pageSize(value = 50): number {
  if (!Number.isInteger(value) || value < 1 || value > 100) {
    return invalid();
  }
  return value;
}

function archivedOption(value = false): boolean {
  if (typeof value !== 'boolean') {
    return invalid();
  }
  return value;
}

function key(value: unknown): string {
  if (typeof value !== 'string' || !/^[a-z0-9_-]{1,48}$/.test(value)) {
    return invalid();
  }
  return value;
}

function common(
  item: Record<string, unknown>,
  projectId: string,
  includeArchived: boolean,
): CatalogCommon {
  if (
    uuid(item.project_id) !== uuid(projectId) ||
    typeof item.name !== 'string' ||
    item.name.trim().length === 0
  ) {
    return invalid();
  }
  // Reject lone surrogates rather than silently replacing persisted text.
  for (const scalar of item.name) {
    const code = scalar.charCodeAt(0);
    if (scalar.length === 1 && code >= 0xd800 && code <= 0xdfff) {
      return invalid();
    }
  }
  const result: CatalogCommon = {
    id: uuid(item.id),
    project_id: projectId,
    name: item.name,
  };
  if ('system_key' in item) {
    result.system_key = key(item.system_key);
  }
  if ('archived_at' in item) {
    const timestamp = item.archived_at;
    if (
      !includeArchived ||
      typeof timestamp !== 'string' ||
      !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z$/.test(timestamp) ||
      !Number.isFinite(Date.parse(timestamp)) ||
      new Date(timestamp).toISOString().slice(0, 19) !== timestamp.slice(0, 19)
    ) {
      return invalid();
    }
    result.archived_at = timestamp;
  }
  return result;
}

function status(
  value: unknown,
  projectId: string,
  includeArchived: boolean,
): ProjectStatus {
  const item = fields(value, [
    ...commonFields,
    'category',
    'position',
    'board_visible',
  ]);
  const category = item.category;
  const position = item.position;
  if (
    (category !== 'backlog' &&
      category !== 'active' &&
      category !== 'done' &&
      category !== 'canceled') ||
    typeof position !== 'string' ||
    !/^(0|[1-9][0-9]{0,18})$/.test(position) ||
    BigInt(position) > 9223372036854775807n ||
    typeof item.board_visible !== 'boolean'
  ) {
    return invalid();
  }
  return {
    ...common(item, projectId, includeArchived),
    category,
    position,
    board_visible: item.board_visible,
  };
}

function issueType(
  value: unknown,
  projectId: string,
  includeArchived: boolean,
): ProjectIssueType {
  const item = fields(value, [...commonFields, 'icon_key', 'color_key']);
  const result = common(item, projectId, includeArchived);
  const parsed: ProjectIssueType = result;
  if ('icon_key' in item) {
    parsed.icon_key = key(item.icon_key);
  }
  if ('color_key' in item) {
    parsed.color_key = key(item.color_key);
  }
  return parsed;
}

function page<T extends CatalogCommon>(
  value: unknown,
  projectId: string,
  options: CatalogOptions,
  parse: (raw: unknown, projectId: string, includeArchived: boolean) => T,
): IssuePage<T> {
  uuid(projectId);
  const size = pageSize(options.page_size);
  const includeArchived = archivedOption(options.include_archived);
  let previous = options.cursor === undefined ? '' : uuid(options.cursor);
  const item = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(item.data) ||
    item.data.length > size ||
    typeof item.has_more !== 'boolean' ||
    (item.has_more && item.data.length !== size) ||
    (!item.has_more && 'next_cursor' in item)
  ) {
    return invalid();
  }
  const data: T[] = [];
  for (const raw of item.data) {
    const record = parse(raw, projectId, includeArchived);
    if (record.id <= previous) {
      return invalid();
    }
    previous = record.id;
    data.push(record);
  }
  if (item.has_more) {
    const next = uuid(item.next_cursor);
    if (next !== previous) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: next };
  }
  return { data, has_more: false };
}

export function parseStatusPage(
  value: unknown,
  projectId: string,
  options: CatalogOptions,
): IssuePage<ProjectStatus> {
  return page(value, projectId, options, status);
}

export function parseIssueTypePage(
  value: unknown,
  projectId: string,
  options: CatalogOptions,
): IssuePage<ProjectIssueType> {
  return page(value, projectId, options, issueType);
}

function path(
  projectId: string,
  endpoint: 'statuses' | 'issue-types',
  options: CatalogOptions,
): string {
  uuid(projectId);
  const query = new URLSearchParams({
    page_size: String(pageSize(options.page_size)),
    include_archived: String(archivedOption(options.include_archived)),
  });
  if (options.cursor !== undefined) {
    query.set('cursor', uuid(options.cursor));
  }
  return `/api/v1/projects/${projectId}/${endpoint}?${query}`;
}

export async function getStatusPage(
  projectId: string,
  options: CatalogOptions,
  signal: AbortSignal,
): Promise<IssuePage<ProjectStatus>> {
  const selected = { ...options };
  return parseStatusPage(
    await readIssueJson(path(projectId, 'statuses', selected), signal),
    projectId,
    selected,
  );
}

export async function getIssueTypePage(
  projectId: string,
  options: CatalogOptions,
  signal: AbortSignal,
): Promise<IssuePage<ProjectIssueType>> {
  const selected = { ...options };
  return parseIssueTypePage(
    await readIssueJson(path(projectId, 'issue-types', selected), signal),
    projectId,
    selected,
  );
}
