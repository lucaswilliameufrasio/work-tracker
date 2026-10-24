import { invalid, uuid } from './issue-transport.ts';
import {
  getProjectPage,
  getProjectCapabilities,
  projectKey,
  type AccessibleProject,
} from './projects.ts';
import { getCurrentSession } from './sessions.ts';
import {
  getIssueDetail,
  getIssueHistoryPage,
  getIssuePage,
  getIssueSearchPage,
  parseIssueHistoryPage,
  IssueRequestError,
  validateIssueSearchCursor,
  validateIssueSearchPriority,
  validateIssueSearchText,
  type IssueDetail,
  type IssueHistoryEvent,
  type IssuePage,
  type IssuePriority,
  type IssueSummary,
} from './issues.ts';
import {
  getIssueTypeRecord,
  getStatusRecord,
  type ProjectIssueType,
  type ProjectStatus,
} from './project-catalogs.ts';
import {
  boardBoundary,
  boardStatusBoundary,
  getBoardPage,
  getBoardStatusPage,
  type BoardPage,
} from './board.ts';

export type WorkRoute =
  | { type: 'board'; projectId: string; statusCursor?: string }
  | { type: 'column'; projectId: string; statusId: string; cursor?: string }
  | { type: 'projects'; cursor?: string; includeArchived: boolean }
  | { type: 'issues'; projectId: string; cursor?: string }
  | {
      type: 'search';
      projectId: string;
      query?: string;
      priority?: IssuePriority;
      cursor?: string;
    }
  | {
      type: 'detail';
      projectId: string;
      number: string;
      historyCursor?: string;
      from?: string;
      fromSearchQuery?: string;
      fromSearchPriority?: IssuePriority;
      fromSearchCursor?: string;
    }
  | { type: 'invalid' };

function number(value: string): string {
  if (
    !/^[1-9][0-9]{0,18}$/.test(value) ||
    BigInt(value) > 9223372036854775807n
  ) {
    return invalid();
  }
  return value;
}

export function workRoute(path: string, search: string): WorkRoute {
  try {
    const query = new URLSearchParams(search);
    const values = new Map<string, string>();
    for (const [key, value] of query) {
      if (values.has(key)) {
        return { type: 'invalid' };
      }
      values.set(key, value);
    }
    if (path === '/projects') {
      if (
        [...values.keys()].some(
          (key) => !['cursor', 'include_archived'].includes(key),
        )
      ) {
        return { type: 'invalid' };
      }
      const archived = values.get('include_archived') ?? 'false';
      if (archived !== 'true' && archived !== 'false') {
        return { type: 'invalid' };
      }
      const cursor = values.get('cursor');
      return {
        type: 'projects',
        includeArchived: archived === 'true',
        ...(cursor === undefined ? {} : { cursor: projectKey(cursor) }),
      };
    }
    const parts = path.split('/');
    if (parts.length < 4 || parts[1] !== 'projects') {
      return { type: 'invalid' };
    }
    const projectId = uuid(parts[2]);
    if (parts[3] === 'board') {
      if (
        parts.length === 4 &&
        [...values.keys()].every((key) => key === 'status_cursor')
      ) {
        const cursor = values.get('status_cursor');
        if (cursor !== undefined) {
          boardStatusBoundary(cursor);
        }
        return {
          type: 'board',
          projectId,
          ...(cursor === undefined ? {} : { statusCursor: cursor }),
        };
      }
      if (
        parts.length === 5 &&
        [...values.keys()].every((key) => key === 'cursor')
      ) {
        const cursor = values.get('cursor');
        if (cursor !== undefined) {
          boardBoundary(cursor);
        }
        return {
          type: 'column',
          projectId,
          statusId: uuid(parts[4]),
          ...(cursor === undefined ? {} : { cursor }),
        };
      }
      return { type: 'invalid' };
    }
    if (parts[3] !== 'issues') {
      return { type: 'invalid' };
    }
    if (parts.length === 4) {
      if (
        [...values.keys()].some(
          (key) => !['cursor', 'q', 'priority'].includes(key),
        )
      ) {
        return { type: 'invalid' };
      }
      const cursor = values.get('cursor');
      const search = values.get('q');
      const searchPriority = values.get('priority');
      const parsedPriority =
        searchPriority === undefined
          ? undefined
          : validateIssueSearchPriority(searchPriority);
      if (search !== undefined || searchPriority !== undefined) {
        if (search !== undefined) {
          validateIssueSearchText(search);
        }
        if (cursor !== undefined) {
          validateIssueSearchCursor(cursor);
        }
        return {
          type: 'search',
          projectId,
          ...(search === undefined ? {} : { query: search }),
          ...(parsedPriority === undefined ? {} : { priority: parsedPriority }),
          ...(cursor === undefined ? {} : { cursor }),
        };
      }
      if (cursor !== undefined) {
        return {
          type: 'issues',
          projectId,
          cursor: number(cursor),
        };
      }
      return {
        type: 'issues',
        projectId,
      };
    }
    if (
      parts.length !== 5 ||
      parts[4] === undefined ||
      [...values.keys()].some(
        (key) =>
          ![
            'history_cursor',
            'from',
            'from_q',
            'from_cursor',
            'from_priority',
          ].includes(key),
      )
    ) {
      return { type: 'invalid' };
    }
    const history = values.get('history_cursor');
    // Full canonical history cursor validation remains at the client boundary.
    if (history !== undefined && !/^[A-Za-z0-9_-]{90}$/.test(history)) {
      return { type: 'invalid' };
    }
    if (history !== undefined) {
      parseIssueHistoryPage(
        { data: [], has_more: false },
        projectId,
        20,
        history,
      );
    }
    const from = values.get('from');
    const fromSearchQuery = values.get('from_q');
    const fromSearchPriority = values.get('from_priority');
    const fromSearchCursor = values.get('from_cursor');
    const hasSearchContext =
      fromSearchQuery !== undefined || fromSearchPriority !== undefined;
    if (
      (from !== undefined && hasSearchContext) ||
      (fromSearchCursor !== undefined && !hasSearchContext)
    ) {
      return { type: 'invalid' };
    }
    if (fromSearchQuery !== undefined) {
      validateIssueSearchText(fromSearchQuery);
    }
    if (fromSearchPriority !== undefined) {
      validateIssueSearchPriority(fromSearchPriority);
    }
    if (fromSearchCursor !== undefined) {
      validateIssueSearchCursor(fromSearchCursor);
    }
    return {
      type: 'detail',
      projectId,
      number: number(parts[4]),
      ...(history === undefined ? {} : { historyCursor: history }),
      ...(from === undefined ? {} : { from: number(from) }),
      ...(fromSearchQuery === undefined ? {} : { fromSearchQuery }),
      ...(fromSearchPriority === undefined
        ? {}
        : {
            fromSearchPriority: validateIssueSearchPriority(fromSearchPriority),
          }),
      ...(fromSearchCursor === undefined ? {} : { fromSearchCursor }),
    };
  } catch {
    return { type: 'invalid' };
  }
}

export type WorkData =
  | {
      type: 'board';
      projectId: string;
      columns: { status: ProjectStatus; page: BoardPage }[];
      actorId: string;
      canMoveIssues: boolean;
      nextStatusCursor?: string;
    }
  | { type: 'column'; status: ProjectStatus; page: BoardPage }
  | { type: 'projects'; page: IssuePage<AccessibleProject> }
  | { type: 'issues'; page: IssuePage<IssueSummary> }
  | {
      type: 'search';
      projectId: string;
      query?: string;
      priority?: IssuePriority;
      page: IssuePage<IssueSummary>;
    }
  | {
      type: 'detail';
      issue: IssueDetail;
      history: IssuePage<IssueHistoryEvent>;
      status: ProjectStatus;
      issueType: ProjectIssueType;
      actorId: string;
      canUpdateIssueFields: boolean;
    };

export async function loadWork(
  route: WorkRoute,
  signal: AbortSignal,
): Promise<WorkData> {
  if (route.type === 'board') {
    const [statuses, capability, session] = await Promise.all([
      getBoardStatusPage(
        route.projectId,
        { page_size: 4, cursor: route.statusCursor },
        signal,
      ),
      getProjectCapabilities(route.projectId, signal),
      getCurrentSession(signal),
    ]);
    const columns = await Promise.all(
      statuses.data.map(async (status) => ({
        status,
        page: await getBoardPage(
          route.projectId,
          status.id,
          { page_size: 20 },
          signal,
        ),
      })),
    );
    return {
      type: 'board',
      projectId: route.projectId,
      columns,
      actorId: session.user_id,
      canMoveIssues: capability.can_move_issues,
      ...(statuses.next_cursor === undefined
        ? {}
        : { nextStatusCursor: statuses.next_cursor }),
    };
  }
  if (route.type === 'column') {
    const [status, page] = await Promise.all([
      getStatusRecord(route.projectId, route.statusId, signal),
      getBoardPage(
        route.projectId,
        route.statusId,
        { cursor: route.cursor, page_size: 20 },
        signal,
      ),
    ]);
    return { type: 'column', status, page };
  }
  if (route.type === 'projects') {
    return {
      type: 'projects',
      page: await getProjectPage(route.cursor, route.includeArchived, signal),
    };
  }
  if (route.type === 'issues') {
    return {
      type: 'issues',
      page: await getIssuePage(
        route.projectId,
        { cursor: route.cursor, page_size: 20 },
        signal,
      ),
    };
  }
  if (route.type === 'search') {
    return {
      type: 'search',
      projectId: route.projectId,
      ...(route.query === undefined ? {} : { query: route.query }),
      page: await getIssueSearchPage(
        route.projectId,
        route.query,
        { cursor: route.cursor, page_size: 20 },
        signal,
        route.priority,
      ),
      ...(route.priority === undefined ? {} : { priority: route.priority }),
    };
  }
  if (route.type === 'invalid') {
    return invalid();
  }
  const issue = await getIssueDetail(route.projectId, route.number, signal);
  const [history, status, issueType, capability, session] = await Promise.all([
    getIssueHistoryPage(
      route.projectId,
      route.number,
      { cursor: route.historyCursor, page_size: 20 },
      signal,
    ),
    getStatusRecord(route.projectId, issue.status_id, signal),
    getIssueTypeRecord(route.projectId, issue.issue_type_id, signal),
    getProjectCapabilities(route.projectId, signal),
    getCurrentSession(signal),
  ]);
  for (const event of history.data) {
    if (event.issue_id !== issue.id) {
      return invalid();
    }
  }
  return {
    type: 'detail',
    issue,
    history,
    status,
    issueType,
    actorId: session.user_id,
    canUpdateIssueFields: capability.can_update_issue_fields,
  };
}

export type WorkState =
  | { type: 'idle' }
  | { type: 'loading' }
  | { type: 'suspended' }
  | { type: 'ready'; data: WorkData }
  | { type: 'signed-out' }
  | { type: 'unavailable' }
  | { type: 'read-error' };

export class WorkScreen {
  private current: WorkState = { type: 'idle' };
  private readonly lifetime = new AbortController();
  private operation = new AbortController();
  private generation = 0;
  private readonly route: WorkRoute;
  private readonly changed: (view: WorkState) => void;
  private readonly load: typeof loadWork;

  constructor(
    route: WorkRoute,
    changed: (view: WorkState) => void,
    load: typeof loadWork = loadWork,
  ) {
    this.route = route;
    this.changed = changed;
    this.load = load;
  }

  get state(): WorkState {
    return this.current;
  }

  dispose(): void {
    this.lifetime.abort();
    this.operation.abort();
    this.generation += 1;
    this.current = { type: 'suspended' };
  }

  suspend(): void {
    if (this.lifetime.signal.aborted) {
      return;
    }
    this.operation.abort();
    this.generation += 1;
    this.set({ type: 'suspended' });
  }

  sessionEnded(): void {
    if (this.lifetime.signal.aborted) {
      return;
    }
    this.operation.abort();
    this.generation += 1;
    this.set({ type: 'signed-out' });
  }

  async start(): Promise<void> {
    if (this.current.type === 'idle' || this.current.type === 'suspended') {
      await this.refresh();
    }
  }

  private set(view: WorkState): void {
    if (!this.lifetime.signal.aborted) {
      this.current = view;
      this.changed(view);
    }
  }

  async refresh(): Promise<void> {
    if (this.lifetime.signal.aborted || this.current.type === 'loading') {
      return;
    }
    if (this.route.type === 'invalid') {
      this.set({ type: 'unavailable' });
      return;
    }
    this.operation.abort();
    this.operation = new AbortController();
    const operation = this.operation;
    const generation = ++this.generation;
    const signal = AbortSignal.any([
      this.lifetime.signal,
      operation.signal,
      AbortSignal.timeout(8000),
    ]);
    this.set({ type: 'loading' });
    try {
      const data = await this.load(this.route, signal);
      if (
        !this.lifetime.signal.aborted &&
        generation === this.generation &&
        !signal.aborted
      ) {
        this.set({ type: 'ready', data });
      } else if (
        !this.lifetime.signal.aborted &&
        generation === this.generation
      ) {
        this.set({ type: 'read-error' });
      }
    } catch (error) {
      operation.abort();
      if (this.lifetime.signal.aborted || generation !== this.generation) {
        return;
      }
      if (error instanceof IssueRequestError && error.status === 401) {
        this.set({ type: 'signed-out' });
      } else if (
        error instanceof IssueRequestError &&
        (error.status === 404 || error.status === 403)
      ) {
        this.set({ type: 'unavailable' });
      } else {
        this.set({ type: 'read-error' });
      }
    }
  }
}
