import {
  fields,
  invalid,
  readIssueJson,
  record,
  uuid,
} from './issue-transport.ts';
import { parseIssueSummary, type IssueSummary } from './issues.ts';
import type { IssuePage } from './issues.ts';
import { parseStatusRecord, type ProjectStatus } from './project-catalogs.ts';

export type BoardCard = IssueSummary & { rank: string };
export type BoardPage = {
  data: BoardCard[];
  total_count: string;
  has_more: boolean;
  next_cursor?: string;
};
export type BoardOptions = { cursor?: string; page_size?: number };

function rank(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^(0|-?[1-9][0-9]{0,18})$/.test(value) ||
    BigInt(value) < -9223372036854775808n ||
    BigInt(value) > 9223372036854775807n
  ) {
    return invalid();
  }
  return value;
}

export function boardBoundary(value: string): { rank: string; id: string } {
  if (!/^[A-Za-z0-9_-]{51,76}$/.test(value)) {
    return invalid();
  }
  const raw = atob(value.replaceAll('-', '+').replaceAll('_', '/'));
  if (
    btoa(raw).replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '') !==
    value
  ) {
    return invalid();
  }
  const parts = raw.split('/');
  if (parts.length !== 2) {
    return invalid();
  }
  return { rank: rank(parts[0]), id: uuid(parts[1]) };
}

function size(value = 20): number {
  if (!Number.isInteger(value) || value < 1 || value > 100) {
    return invalid();
  }
  return value;
}

export function parseBoardPage(
  value: unknown,
  projectId: string,
  statusId: string,
  options: BoardOptions,
): BoardPage {
  uuid(projectId);
  uuid(statusId);
  const pageSize = size(options.page_size);
  let previous =
    options.cursor === undefined ? undefined : boardBoundary(options.cursor);
  const page = fields(value, [
    'data',
    'total_count',
    'has_more',
    'next_cursor',
  ]);
  const total = page.total_count;
  if (
    typeof total !== 'string' ||
    !/^(0|[1-9][0-9]{0,18})$/.test(total) ||
    BigInt(total) > 9223372036854775807n ||
    !Array.isArray(page.data) ||
    page.data.length > pageSize ||
    BigInt(total) < BigInt(page.data.length) ||
    typeof page.has_more !== 'boolean' ||
    (page.has_more &&
      (page.data.length !== pageSize ||
        BigInt(total) <= BigInt(page.data.length))) ||
    (!page.has_more && 'next_cursor' in page)
  ) {
    return invalid();
  }
  const data: BoardCard[] = [];
  const ids = new Set<string>();
  for (const raw of page.data) {
    if (!record(raw)) {
      return invalid();
    }
    const { rank: rawRank, ...rest } = raw;
    const summary = parseIssueSummary(rest, projectId);
    const parsedRank = rank(rawRank);
    if (
      summary.status_id !== statusId ||
      ids.has(summary.id) ||
      (previous !== undefined &&
        (BigInt(parsedRank) < BigInt(previous.rank) ||
          (parsedRank === previous.rank && summary.id <= previous.id)))
    ) {
      return invalid();
    }
    previous = { rank: parsedRank, id: summary.id };
    ids.add(summary.id);
    data.push({ ...summary, rank: parsedRank });
  }
  if (page.has_more) {
    if (typeof page.next_cursor !== 'string' || previous === undefined) {
      return invalid();
    }
    const next = boardBoundary(page.next_cursor);
    if (next.rank !== previous.rank || next.id !== previous.id) {
      return invalid();
    }
    return {
      data,
      total_count: total,
      has_more: true,
      next_cursor: page.next_cursor,
    };
  }
  return { data, total_count: total, has_more: false };
}

export async function getBoardPage(
  projectId: string,
  statusId: string,
  options: BoardOptions,
  signal: AbortSignal,
): Promise<BoardPage> {
  uuid(projectId);
  uuid(statusId);
  const selected = { ...options };
  const query = new URLSearchParams({
    page_size: String(size(selected.page_size)),
  });
  if (selected.cursor !== undefined) {
    boardBoundary(selected.cursor);
    query.set('cursor', selected.cursor);
  }
  return parseBoardPage(
    await readIssueJson(
      `/api/v1/projects/${projectId}/board/${statusId}?${query}`,
      signal,
    ),
    projectId,
    statusId,
    selected,
  );
}

export function boardStatusBoundary(value: string): {
  position: string;
  id: string;
} {
  const selected = boardBoundary(value);
  if (BigInt(selected.rank) < 0) {
    return invalid();
  }
  return { position: selected.rank, id: selected.id };
}

export function parseBoardStatusPage(
  value: unknown,
  projectId: string,
  options: BoardOptions,
): IssuePage<ProjectStatus> {
  uuid(projectId);
  const pageSize = size(options.page_size ?? 4);
  let previous =
    options.cursor === undefined
      ? undefined
      : boardStatusBoundary(options.cursor);
  const page = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(page.data) ||
    page.data.length > pageSize ||
    typeof page.has_more !== 'boolean' ||
    (page.has_more && page.data.length !== pageSize) ||
    (!page.has_more && 'next_cursor' in page)
  ) {
    return invalid();
  }
  const data: ProjectStatus[] = [];
  const ids = new Set<string>();
  for (const raw of page.data) {
    if (!record(raw)) {
      return invalid();
    }
    const status = parseStatusRecord(raw, projectId, uuid(raw.id));
    if (
      !status.board_visible ||
      status.archived_at !== undefined ||
      ids.has(status.id) ||
      (previous !== undefined &&
        (BigInt(status.position) < BigInt(previous.position) ||
          (status.position === previous.position && status.id <= previous.id)))
    ) {
      return invalid();
    }
    previous = { position: status.position, id: status.id };
    ids.add(status.id);
    data.push(status);
  }
  if (page.has_more) {
    if (typeof page.next_cursor !== 'string' || previous === undefined) {
      return invalid();
    }
    const next = boardStatusBoundary(page.next_cursor);
    if (next.position !== previous.position || next.id !== previous.id) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: page.next_cursor };
  }
  return { data, has_more: false };
}

export async function getBoardStatusPage(
  projectId: string,
  options: BoardOptions,
  signal: AbortSignal,
): Promise<IssuePage<ProjectStatus>> {
  uuid(projectId);
  const selected = { ...options };
  const query = new URLSearchParams({
    page_size: String(size(selected.page_size ?? 4)),
  });
  if (selected.cursor !== undefined) {
    boardStatusBoundary(selected.cursor);
    query.set('cursor', selected.cursor);
  }
  return parseBoardStatusPage(
    await readIssueJson(`/api/v1/projects/${projectId}/board?${query}`, signal),
    projectId,
    selected,
  );
}
