import {
  fields,
  invalid,
  readIssueJson,
  record,
  uuid,
} from './issue-transport.ts';
import { getCurrentSession } from './sessions.ts';

export type ProjectMember = {
  user_id: string;
  display_name: string;
  role: 'manager' | 'member' | 'viewer';
  primary_email?: string;
  can_be_assigned: boolean;
};

export type ProjectMemberPage = {
  data: ProjectMember[];
  has_more: boolean;
  next_cursor?: string;
};

export class ProjectMemberIdentityChanged extends Error {
  constructor() {
    super('Project member identity changed');
    this.name = 'ProjectMemberIdentityChanged';
  }
}

function member(value: unknown): ProjectMember {
  const item = fields(value, [
    'user_id',
    'display_name',
    'role',
    'primary_email',
    'can_be_assigned',
  ]);
  const userId = uuid(item.user_id);
  const displayName = item.display_name;
  const role = item.role;
  const canBeAssigned = item.can_be_assigned;
  if (
    typeof displayName !== 'string' ||
    displayName.trim().length === 0 ||
    (role !== 'manager' && role !== 'member' && role !== 'viewer') ||
    typeof canBeAssigned !== 'boolean' ||
    (canBeAssigned && role === 'viewer')
  ) {
    return invalid();
  }
  if ('primary_email' in item && typeof item.primary_email !== 'string') {
    return invalid();
  }
  return {
    user_id: userId,
    display_name: displayName,
    role,
    ...(item.primary_email === undefined
      ? {}
      : { primary_email: item.primary_email as string }),
    can_be_assigned: canBeAssigned,
  };
}

function page(value: unknown, requestedSize: number): ProjectMemberPage {
  const item = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(item.data) ||
    item.data.length > requestedSize ||
    typeof item.has_more !== 'boolean'
  ) {
    return invalid();
  }
  const data: ProjectMember[] = [];
  for (const raw of item.data) {
    data.push(member(raw));
  }
  for (let index = 1; index < data.length; index += 1) {
    if (data[index - 1].user_id >= data[index].user_id) {
      return invalid();
    }
  }
  if (item.has_more) {
    const nextCursor = uuid(item.next_cursor);
    if (data.length === 0 || nextCursor !== data[data.length - 1].user_id) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: nextCursor };
  }
  if ('next_cursor' in item) {
    return invalid();
  }
  return { data, has_more: false };
}

export async function listProjectMembers(
  projectId: string,
  expectedActorId: string,
  options: { cursor?: string; page_size?: number },
  signal: AbortSignal,
): Promise<ProjectMemberPage> {
  const project = uuid(projectId);
  const actor = uuid(expectedActorId);
  const pageSize = options.page_size ?? 50;
  if (!Number.isInteger(pageSize) || pageSize < 1 || pageSize > 100) {
    return invalid();
  }
  const query = new URLSearchParams({ page_size: String(pageSize) });
  if (options.cursor !== undefined) {
    query.set('cursor', uuid(options.cursor));
  }
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const before = await getCurrentSession(deadline);
  if (before.user_id !== actor) {
    throw new ProjectMemberIdentityChanged();
  }
  const result = await readIssueJson(
    `/api/v1/projects/${project}/members?${query}`,
    deadline,
  );
  const after = await getCurrentSession(deadline);
  if (after.user_id !== actor) {
    throw new ProjectMemberIdentityChanged();
  }
  return page(result, pageSize);
}

export async function getProjectMember(
  projectId: string,
  userId: string,
  expectedActorId: string,
  signal: AbortSignal,
): Promise<ProjectMember> {
  const project = uuid(projectId);
  const user = uuid(userId);
  const actor = uuid(expectedActorId);
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  const before = await getCurrentSession(deadline);
  if (before.user_id !== actor) {
    throw new ProjectMemberIdentityChanged();
  }
  const result = await readIssueJson(
    `/api/v1/projects/${project}/members/${user}`,
    deadline,
  );
  const after = await getCurrentSession(deadline);
  if (after.user_id !== actor) {
    throw new ProjectMemberIdentityChanged();
  }
  if (!record(result)) {
    return invalid();
  }
  return member(result);
}
