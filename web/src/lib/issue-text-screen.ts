import { getCurrentSession, SessionRequestError } from './sessions.ts';
import { getProjectCapabilities } from './projects.ts';
import { getIssueDetail, type IssueDetail } from './issues.ts';
import { IssueRequestError, uuid } from './issue-transport.ts';
import {
  IssueEditIdentityChanged,
  replaceIssueText,
  serializeIssueTextInput,
  type IssueTextReceipt,
  type IssueTextTarget,
} from './issue-text.ts';

export type TextDraft = { title: string; description: string };
export type EditorRoute = { project: string; number: string };
export type EditorState =
  | {
      type:
        | 'idle'
        | 'loading'
        | 'suspended'
        | 'saving'
        | 'uncertain'
        | 'signed-out'
        | 'unavailable'
        | 'read-only'
        | 'read-error';
    }
  | { type: 'editing' | 'conflict'; draft: TextDraft; version: string }
  | { type: 'review'; draft: TextDraft; version: string; current: IssueDetail }
  | { type: 'saved'; receipt: IssueTextReceipt };

type Work =
  | { type: 'none' }
  | {
      type: 'draft' | 'conflict';
      draft: TextDraft;
      version: string;
      target: IssueTextTarget;
    }
  | {
      type: 'command';
      draft: TextDraft;
      version: string;
      target: IssueTextTarget;
      key: string;
    }
  | {
      type: 'review';
      draft: TextDraft;
      version: string;
      target: IssueTextTarget;
      current: IssueDetail;
    }
  | { type: 'saved'; receipt: IssueTextReceipt };

class EditorAccessError extends Error {
  constructor(readonly kind: 'signed-out' | 'unavailable' | 'read-only') {
    super('Editor access unavailable');
  }
}

export function editorRoute(
  path: string,
  search: string,
): EditorRoute | undefined {
  try {
    const parts = path.split('/');
    const number = parts[4];
    if (
      search !== '' ||
      parts.length !== 6 ||
      parts[1] !== 'projects' ||
      parts[3] !== 'issues' ||
      parts[5] !== 'edit' ||
      number === undefined ||
      !/^[1-9][0-9]{0,18}$/.test(number) ||
      BigInt(number) > 9223372036854775807n
    ) {
      return undefined;
    }
    return { project: uuid(parts[2]), number };
  } catch {
    return undefined;
  }
}

export async function loadEditor(
  route: EditorRoute,
  actor: string | undefined,
  signal: AbortSignal,
): Promise<{ actor: string; issue: IssueDetail }> {
  const session = await getCurrentSession(signal);
  if (actor !== undefined && session.user_id !== actor) {
    throw new EditorAccessError('unavailable');
  }
  const capability = await getProjectCapabilities(route.project, signal);
  if (!capability.can_replace_issue_text) {
    throw new EditorAccessError('read-only');
  }
  const issue = await getIssueDetail(route.project, route.number, signal);
  if (issue.trashed_at !== undefined) {
    throw new EditorAccessError('read-only');
  }
  const checked = await getCurrentSession(signal);
  if (checked.user_id !== session.user_id) {
    throw new EditorAccessError('unavailable');
  }
  return { actor: session.user_id, issue };
}

type Dependencies = {
  load: typeof loadEditor;
  send: typeof replaceIssueText;
  key: () => string;
};
export class IssueTextScreen {
  private current: EditorState = { type: 'idle' };
  private work: Work = { type: 'none' };
  private actor: string | undefined;
  private readonly lifetime = new AbortController();
  private operation = new AbortController();
  private generation = 0;

  constructor(
    private readonly route: EditorRoute | undefined,
    private readonly changed: (state: EditorState) => void,
    private readonly deps: Dependencies = {
      load: loadEditor,
      send: replaceIssueText,
      key: () => crypto.randomUUID(),
    },
  ) {}
  get state(): EditorState {
    return this.current;
  }
  private set(state: EditorState): void {
    if (!this.lifetime.signal.aborted) {
      this.current = state;
      this.changed(state);
    }
  }
  suspend(): void {
    if (!this.lifetime.signal.aborted) {
      this.operation.abort();
      this.generation += 1;
      this.set({ type: 'suspended' });
    }
  }
  dispose(): void {
    this.suspend();
    this.lifetime.abort();
    this.work = { type: 'none' };
    this.actor = undefined;
  }
  private begin(): { generation: number; signal: AbortSignal } {
    this.operation.abort();
    this.operation = new AbortController();
    return {
      generation: ++this.generation,
      signal: AbortSignal.any([
        this.lifetime.signal,
        this.operation.signal,
        AbortSignal.timeout(8000),
      ]),
    };
  }
  private alive(generation: number): boolean {
    return !this.lifetime.signal.aborted && generation === this.generation;
  }
  private denied(
    error: unknown,
  ): 'signed-out' | 'unavailable' | 'read-only' | undefined {
    if (error instanceof IssueEditIdentityChanged) {
      return 'unavailable';
    }
    if (error instanceof EditorAccessError) {
      return error.kind;
    }
    if (
      error instanceof IssueRequestError ||
      error instanceof SessionRequestError
    ) {
      if (error.status === 401) {
        return 'signed-out';
      }
      if (error.status === 403 || error.status === 404) {
        return 'unavailable';
      }
    }
    return undefined;
  }
  async start(): Promise<void> {
    if (
      [
        'idle',
        'suspended',
        'signed-out',
        'unavailable',
        'read-only',
        'read-error',
      ].includes(this.current.type)
    ) {
      await this.load('resume');
    }
  }
  private async load(action: 'resume' | 'review'): Promise<void> {
    if (this.lifetime.signal.aborted) {
      return;
    }
    if (this.route === undefined) {
      this.work = { type: 'none' };
      this.set({ type: 'unavailable' });
      return;
    }
    const { generation, signal } = this.begin();
    this.set({ type: 'loading' });
    try {
      const result = await this.deps.load(this.route, this.actor, signal);
      signal.throwIfAborted();
      if (!this.alive(generation)) {
        return;
      }
      if (this.actor !== undefined && result.actor !== this.actor) {
        throw new EditorAccessError('unavailable');
      }
      this.actor = uuid(result.actor);
      const issue = result.issue;
      const target = { id: issue.id, issue_number: issue.issue_number };
      const work = this.work;
      if (
        work.type !== 'none' &&
        (work.type === 'saved' ? work.receipt.id : work.target.id) !== issue.id
      ) {
        throw new EditorAccessError('unavailable');
      }
      if (work.type === 'command') {
        this.set({ type: 'uncertain' });
        return;
      }
      if (work.type === 'saved') {
        this.set({ type: 'saved', receipt: work.receipt });
        return;
      }
      if (work.type === 'none') {
        const draft = {
          title: issue.title,
          description: issue.description ?? '',
        };
        this.work = { type: 'draft', draft, version: issue.version, target };
        this.set({ type: 'editing', draft, version: issue.version });
        return;
      }
      if (action === 'review' || work.type === 'review') {
        this.work = {
          type: 'review',
          draft: work.draft,
          version: work.version,
          target,
          current: issue,
        };
        this.set({
          type: 'review',
          draft: work.draft,
          version: work.version,
          current: issue,
        });
        return;
      }
      const conflict =
        work.type === 'conflict' || work.version !== issue.version;
      this.work = {
        type: conflict ? 'conflict' : 'draft',
        draft: work.draft,
        version: work.version,
        target,
      };
      this.set({
        type: conflict ? 'conflict' : 'editing',
        draft: work.draft,
        version: work.version,
      });
    } catch (error) {
      if (!this.alive(generation)) {
        return;
      }
      const denied = this.denied(error);
      if (denied !== undefined) {
        this.work = { type: 'none' };
        this.set({ type: denied });
      } else {
        this.set({ type: 'read-error' });
      }
    }
  }
  change(draft: TextDraft): void {
    if (this.current.type !== 'editing' || this.work.type !== 'draft') {
      return;
    }
    const snapshot = { title: draft.title, description: draft.description };
    this.work = { ...this.work, draft: snapshot };
    this.set({ type: 'editing', draft: snapshot, version: this.work.version });
  }
  async save(): Promise<void> {
    if (this.current.type !== 'editing' || this.work.type !== 'draft') {
      return;
    }
    const work = this.work;
    serializeIssueTextInput({
      title: work.draft.title,
      ...(work.draft.description === ''
        ? {}
        : { description: work.draft.description }),
      expected_version: work.version,
    });
    this.work = {
      ...work,
      type: 'command',
      draft: { ...work.draft },
      key: uuid(this.deps.key()),
    };
    await this.send();
  }
  async retry(): Promise<void> {
    if (this.current.type === 'uncertain' && this.work.type === 'command') {
      await this.send();
    }
  }
  async review(): Promise<void> {
    if (this.current.type === 'conflict' && this.work.type === 'conflict') {
      await this.load('review');
    }
  }
  choose(source: 'draft' | 'current'): void {
    if (this.current.type !== 'review' || this.work.type !== 'review') {
      return;
    }
    const work = this.work;
    const draft =
      source === 'draft'
        ? { ...work.draft }
        : {
            title: work.current.title,
            description: work.current.description ?? '',
          };
    this.work = {
      type: 'draft',
      draft,
      version: work.current.version,
      target: work.target,
    };
    this.set({ type: 'editing', draft, version: work.current.version });
  }
  private async send(): Promise<void> {
    if (
      this.work.type !== 'command' ||
      this.route === undefined ||
      this.actor === undefined ||
      this.lifetime.signal.aborted
    ) {
      return;
    }
    const work = this.work;
    const { generation, signal } = this.begin();
    this.set({ type: 'saving' });
    try {
      const result = await this.deps.send(
        this.route.project,
        work.target,
        this.actor,
        work.key,
        {
          title: work.draft.title,
          ...(work.draft.description === ''
            ? {}
            : { description: work.draft.description }),
          expected_version: work.version,
        },
        signal,
      );
      signal.throwIfAborted();
      if (!this.alive(generation)) {
        return;
      }
      this.work = { type: 'saved', receipt: result.receipt };
      this.set({ type: 'saved', receipt: result.receipt });
    } catch (error) {
      if (!this.alive(generation)) {
        return;
      }
      const denied = this.denied(error);
      if (denied !== undefined) {
        this.work = { type: 'none' };
        this.set({ type: denied });
      } else if (
        error instanceof IssueRequestError &&
        error.status === 409 &&
        error.code === 'issue_version_conflict'
      ) {
        this.work = {
          type: 'conflict',
          draft: work.draft,
          version: work.version,
          target: work.target,
        };
        this.set({
          type: 'conflict',
          draft: work.draft,
          version: work.version,
        });
      } else {
        this.set({ type: 'uncertain' });
      }
    }
  }
}
