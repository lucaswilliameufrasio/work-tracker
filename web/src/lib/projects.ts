import { fields, invalid, readIssueJson, uuid } from './issue-transport.ts';
import type { IssuePage } from './issues.ts';

export type AccessibleProject = {
  id: string;
  project_key: string;
  name: string;
  role: 'manager' | 'member' | 'viewer';
  archived_at?: string;
};

export type ProjectCapabilities = {
  project_id: string;
  can_create_issues: boolean;
  can_replace_issue_text: boolean;
};

export function parseProjectCapabilities(
  value: unknown,
  projectId: string,
): ProjectCapabilities {
  const item = fields(value, [
    'project_id',
    'can_create_issues',
    'can_replace_issue_text',
  ]);
  const project = uuid(item.project_id);
  if (
    project !== uuid(projectId) ||
    typeof item.can_create_issues !== 'boolean' ||
    (item.can_replace_issue_text !== undefined &&
      typeof item.can_replace_issue_text !== 'boolean')
  ) {
    return invalid();
  }
  return {
    project_id: project,
    can_create_issues: item.can_create_issues,
    can_replace_issue_text: item.can_replace_issue_text === true,
  };
}

export async function getProjectCapabilities(
  projectId: string,
  signal: AbortSignal,
): Promise<ProjectCapabilities> {
  uuid(projectId);
  return parseProjectCapabilities(
    await readIssueJson(`/api/v1/projects/${projectId}/capabilities`, signal),
    projectId,
  );
}

export function projectKey(value: unknown): string {
  if (typeof value !== 'string' || !/^[A-Z]{2,6}$/.test(value)) {
    return invalid();
  }
  return value;
}

export function parseProjectPage(
  value: unknown,
  cursor?: string,
  includeArchived = false,
): IssuePage<AccessibleProject> {
  const page = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(page.data) ||
    page.data.length > 20 ||
    typeof page.has_more !== 'boolean' ||
    (page.has_more && page.data.length !== 20) ||
    (!page.has_more && 'next_cursor' in page)
  ) {
    return invalid();
  }
  let previous = cursor === undefined ? '' : projectKey(cursor);
  const data: AccessibleProject[] = [];
  const ids = new Set<string>();
  for (const raw of page.data) {
    const item = fields(raw, [
      'id',
      'project_key',
      'name',
      'role',
      'archived_at',
    ]);
    const key = projectKey(item.project_key);
    const id = uuid(item.id);
    const role = item.role;
    if (
      key <= previous ||
      ids.has(id) ||
      typeof item.name !== 'string' ||
      item.name.trim().length === 0 ||
      (role !== 'manager' && role !== 'member' && role !== 'viewer')
    ) {
      return invalid();
    }
    const project: AccessibleProject = {
      id,
      project_key: key,
      name: item.name,
      role,
    };
    if ('archived_at' in item) {
      const time = item.archived_at;
      if (
        !includeArchived ||
        typeof time !== 'string' ||
        !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z$/.test(time) ||
        !Number.isFinite(Date.parse(time)) ||
        new Date(time).toISOString().slice(0, 19) !== time.slice(0, 19)
      ) {
        return invalid();
      }
      project.archived_at = time;
    }
    data.push(project);
    ids.add(id);
    previous = key;
  }
  if (page.has_more) {
    const next = projectKey(page.next_cursor);
    if (next !== previous) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: next };
  }
  return { data, has_more: false };
}

export async function getProjectPage(
  cursor: string | undefined,
  includeArchived: boolean,
  signal: AbortSignal,
): Promise<IssuePage<AccessibleProject>> {
  if (typeof includeArchived !== 'boolean') {
    return invalid();
  }
  const query = new URLSearchParams({
    page_size: '20',
    include_archived: String(includeArchived),
  });
  if (cursor !== undefined) {
    query.set('cursor', projectKey(cursor));
  }
  return parseProjectPage(
    await readIssueJson(`/api/v1/projects?${query}`, signal),
    cursor,
    includeArchived,
  );
}
