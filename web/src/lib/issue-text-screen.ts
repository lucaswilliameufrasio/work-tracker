import { getCurrentSession, SessionRequestError } from './sessions.ts';
import { getProjectCapabilities } from './projects.ts';
import { getIssueDetail, type IssueDetail } from './issues.ts';
import { IssueRequestError, uuid } from './issue-transport.ts';
import {
  IssueTextDraftStorageError,
  issueTextDraftStore,
  type IssueTextDraftIdentity,
  type IssueTextDraftRepository,
  type IssueTextDraftTarget,
  type PersistedIssueTextDraft,
} from './issue-text-drafts.ts';
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
  | { type: 'storage-unavailable' }
  | { type: 'editing' | 'conflict'; draft: TextDraft; version: string }
  | { type: 'review'; draft: TextDraft; version: string; current: IssueDetail }
  | { type: 'saved'; receipt: IssueTextReceipt };

type DraftWork = {
  type: 'draft';
  draft: TextDraft;
  version: string;
  target: IssueTextTarget;
};
type ConflictWork = {
  type: 'conflict';
  draft: TextDraft;
  version: string;
  target: IssueTextTarget;
};
type Work =
  | { type: 'none' }
  | DraftWork
  | ConflictWork
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

type StorageRecovery =
  | { type: 'load' }
  | { type: 'save'; state: EditorState }
  | { type: 'clear-all'; state: EditorState }
  | { type: 'clear-project'; project: string; state: EditorState };

class EditorAccessError extends Error {
  constructor(
    readonly kind:
      'signed-out' | 'unavailable' | 'read-only' | 'identity-changed',
  ) {
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
  identity: IssueTextDraftIdentity | undefined,
  signal: AbortSignal,
): Promise<{ actor: string; session_id: string; issue: IssueDetail }> {
  const session = await getCurrentSession(signal);
  if (
    identity !== undefined &&
    (session.user_id !== identity.actor_id ||
      session.session_id !== identity.session_id)
  ) {
    throw new EditorAccessError('identity-changed');
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
  if (
    checked.user_id !== session.user_id ||
    checked.session_id !== session.session_id
  ) {
    throw new EditorAccessError('identity-changed');
  }
  return { actor: session.user_id, session_id: session.session_id, issue };
}

type Dependencies = {
  load: typeof loadEditor;
  send: typeof replaceIssueText;
  key: () => string;
  drafts: IssueTextDraftRepository;
};
export class IssueTextScreen {
  private current: EditorState = { type: 'idle' };
  private work: Work = { type: 'none' };
  private identity: IssueTextDraftIdentity | undefined;
  private storageRecovery: StorageRecovery | undefined;
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
      drafts: issueTextDraftStore,
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
  sessionEnded(): void {
    if (!this.lifetime.signal.aborted) {
      this.operation.abort();
      this.generation += 1;
      this.identity = undefined;
      this.work = { type: 'none' };
      this.storageRecovery = undefined;
      this.clearAllDrafts({ type: 'signed-out' });
    }
  }
  dispose(): void {
    this.suspend();
    this.lifetime.abort();
    this.work = { type: 'none' };
    this.identity = undefined;
    this.storageRecovery = undefined;
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
      if (error.kind === 'identity-changed') {
        return 'unavailable';
      }
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
  async retryStorage(): Promise<void> {
    if (
      this.current.type !== 'storage-unavailable' ||
      this.storageRecovery === undefined
    ) {
      return;
    }
    const recovery = this.storageRecovery;
    try {
      if (recovery.type === 'load') {
        this.storageRecovery = undefined;
        this.set({ type: 'idle' });
        await this.load('resume');
        return;
      }
      if (recovery.type === 'save') {
        this.persistWork();
      } else if (recovery.type === 'clear-all') {
        this.deps.drafts.clearAll();
      } else {
        this.deps.drafts.clearProject(recovery.project);
      }
      this.storageRecovery = undefined;
      this.set(recovery.type === 'save' ? this.stateForWork() : recovery.state);
    } catch (error) {
      if (!(error instanceof IssueTextDraftStorageError)) {
        throw error;
      }
    }
  }
  private persistedTarget(target: IssueTextTarget): IssueTextDraftTarget {
    if (this.route === undefined) {
      throw new IssueTextDraftStorageError();
    }
    return {
      project_id: this.route.project,
      issue_id: target.id,
      issue_number: target.issue_number,
    };
  }
  private persistWork(): void {
    if (this.work.type === 'none') {
      return;
    }
    if (this.work.type === 'saved') {
      if (this.identity === undefined) {
        throw new IssueTextDraftStorageError();
      }
      this.deps.drafts.remove(
        this.identity,
        this.persistedTarget({
          id: this.work.receipt.id,
          issue_number: this.work.receipt.issue_number,
        }),
      );
      return;
    }
    if (this.identity === undefined) {
      throw new IssueTextDraftStorageError();
    }
    const target = this.persistedTarget(this.work.target);
    const value: PersistedIssueTextDraft = {
      schema_version: 1,
      ...this.identity,
      ...target,
      base_version: this.work.version,
      title: this.work.draft.title,
      description: this.work.draft.description,
      ...(this.work.type === 'command'
        ? {
            pending_command: {
              idempotency_key: this.work.key,
              title: this.work.draft.title,
              description: this.work.draft.description,
              expected_version: this.work.version,
            },
          }
        : {}),
    };
    this.deps.drafts.save(value);
  }
  private stateForWork(): EditorState {
    if (this.work.type === 'draft') {
      return {
        type: 'editing',
        draft: this.work.draft,
        version: this.work.version,
      };
    }
    if (this.work.type === 'conflict') {
      return {
        type: 'conflict',
        draft: this.work.draft,
        version: this.work.version,
      };
    }
    if (this.work.type === 'review') {
      return {
        type: 'review',
        draft: this.work.draft,
        version: this.work.version,
        current: this.work.current,
      };
    }
    if (this.work.type === 'command') {
      return { type: 'uncertain' };
    }
    if (this.work.type === 'saved') {
      return { type: 'saved', receipt: this.work.receipt };
    }
    return { type: 'unavailable' };
  }
  private clearAllDrafts(state: EditorState): void {
    try {
      this.deps.drafts.clearAll();
      this.storageRecovery = undefined;
      this.set(state);
    } catch (error) {
      if (!(error instanceof IssueTextDraftStorageError)) {
        throw error;
      }
      this.storageRecovery = { type: 'clear-all', state };
      this.set({ type: 'storage-unavailable' });
    }
  }
  private clearProjectDrafts(project: string, state: EditorState): void {
    try {
      this.deps.drafts.clearProject(project);
      this.storageRecovery = undefined;
      this.set(state);
    } catch (error) {
      if (!(error instanceof IssueTextDraftStorageError)) {
        throw error;
      }
      this.storageRecovery = { type: 'clear-project', project, state };
      this.set({ type: 'storage-unavailable' });
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
      const result = await this.deps.load(this.route, this.identity, signal);
      signal.throwIfAborted();
      if (!this.alive(generation)) {
        return;
      }
      if (
        this.identity !== undefined &&
        (result.actor !== this.identity.actor_id ||
          result.session_id !== this.identity.session_id)
      ) {
        throw new EditorAccessError('identity-changed');
      }
      this.identity = {
        actor_id: uuid(result.actor),
        session_id: uuid(result.session_id),
      };
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
        const stored = this.deps.drafts.load(this.identity, {
          project_id: issue.project_id,
          issue_id: issue.id,
          issue_number: issue.issue_number,
        });
        if (stored !== undefined) {
          const storedTarget = {
            id: stored.issue_id,
            issue_number: stored.issue_number,
          };
          if (stored.pending_command !== undefined) {
            const command = stored.pending_command;
            this.work = {
              type: 'command',
              draft: { title: command.title, description: command.description },
              version: command.expected_version,
              target: storedTarget,
              key: command.idempotency_key,
            };
            this.set({ type: 'uncertain' });
            return;
          }
          const conflict = stored.base_version !== issue.version;
          this.work = {
            type: conflict ? 'conflict' : 'draft',
            draft: { title: stored.title, description: stored.description },
            version: stored.base_version,
            target: storedTarget,
          };
          this.set({
            type: conflict ? 'conflict' : 'editing',
            draft: { title: stored.title, description: stored.description },
            version: stored.base_version,
          });
          return;
        }
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
      if (error instanceof IssueTextDraftStorageError) {
        this.storageRecovery = { type: 'load' };
        this.set({ type: 'storage-unavailable' });
        return;
      }
      if (
        error instanceof EditorAccessError &&
        error.kind === 'identity-changed'
      ) {
        this.work = { type: 'none' };
        this.identity = undefined;
        this.clearAllDrafts({ type: 'unavailable' });
        return;
      }
      const denied = this.denied(error);
      if (denied !== undefined) {
        if (denied === 'signed-out') {
          this.identity = undefined;
          this.work = { type: 'none' };
          this.clearAllDrafts({ type: denied });
        } else if (this.route !== undefined) {
          this.work = { type: 'none' };
          this.clearProjectDrafts(this.route.project, { type: denied });
        } else {
          this.work = { type: 'none' };
          this.set({ type: denied });
        }
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
    try {
      this.persistWork();
      this.set({
        type: 'editing',
        draft: snapshot,
        version: this.work.version,
      });
    } catch (error) {
      if (!(error instanceof IssueTextDraftStorageError)) {
        throw error;
      }
      this.storageRecovery = {
        type: 'save',
        state: { type: 'editing', draft: snapshot, version: this.work.version },
      };
      this.set({ type: 'storage-unavailable' });
    }
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
    const command: Extract<Work, { type: 'command' }> = {
      ...work,
      type: 'command',
      draft: { ...work.draft },
      key: uuid(this.deps.key()),
    };
    this.work = command;
    try {
      this.persistWork();
    } catch (error) {
      if (!(error instanceof IssueTextDraftStorageError)) {
        throw error;
      }
      this.work = work;
      this.storageRecovery = {
        type: 'save',
        state: { type: 'editing', draft: work.draft, version: work.version },
      };
      this.set({ type: 'storage-unavailable' });
      return;
    }
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
    const next: Extract<Work, { type: 'draft' }> = {
      type: 'draft',
      draft,
      version: work.current.version,
      target: work.target,
    };
    this.work = next;
    try {
      this.persistWork();
      this.set({ type: 'editing', draft, version: work.current.version });
    } catch (error) {
      if (!(error instanceof IssueTextDraftStorageError)) {
        throw error;
      }
      this.storageRecovery = {
        type: 'save',
        state: { type: 'editing', draft, version: work.current.version },
      };
      this.set({ type: 'storage-unavailable' });
    }
  }
  private async send(): Promise<void> {
    if (
      this.work.type !== 'command' ||
      this.route === undefined ||
      this.identity === undefined ||
      this.lifetime.signal.aborted
    ) {
      return;
    }
    const identity = this.identity;
    const work = this.work;
    const { generation, signal } = this.begin();
    this.set({ type: 'saving' });
    try {
      const result = await this.deps.send(
        this.route.project,
        work.target,
        identity.actor_id,
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
      try {
        this.deps.drafts.remove(identity, this.persistedTarget(work.target));
      } catch (error) {
        if (!(error instanceof IssueTextDraftStorageError)) {
          throw error;
        }
        this.storageRecovery = { type: 'save', state: { type: 'uncertain' } };
        this.set({ type: 'storage-unavailable' });
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
        if (denied === 'signed-out') {
          this.identity = undefined;
          this.clearAllDrafts({ type: denied });
        } else if (error instanceof IssueEditIdentityChanged) {
          this.identity = undefined;
          this.clearAllDrafts({ type: 'unavailable' });
        } else {
          this.clearProjectDrafts(this.route.project, { type: denied });
        }
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
        try {
          this.persistWork();
          this.set({
            type: 'conflict',
            draft: work.draft,
            version: work.version,
          });
        } catch (storageError) {
          if (!(storageError instanceof IssueTextDraftStorageError)) {
            throw storageError;
          }
          this.storageRecovery = {
            type: 'save',
            state: {
              type: 'conflict',
              draft: work.draft,
              version: work.version,
            },
          };
          this.set({ type: 'storage-unavailable' });
        }
      } else {
        this.set({ type: 'uncertain' });
      }
    }
  }
}
