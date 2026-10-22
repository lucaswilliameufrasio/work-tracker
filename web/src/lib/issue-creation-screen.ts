import {
  createIssue,
  serializeIssueCreationInput,
  type IssueCreationInput,
  type IssueCreationReceipt,
} from './issue-creation.ts';
import { uuid } from './issue-transport.ts';
import { getCurrentSession, SessionRequestError } from './sessions.ts';

export type CreationState =
  | { type: 'idle' }
  | { type: 'checking' }
  | { type: 'suspended' }
  | { type: 'editing' }
  | { type: 'submitting' }
  | { type: 'uncertain' }
  | { type: 'confirmed'; receipt: IssueCreationReceipt }
  | { type: 'signed-out' }
  | { type: 'unavailable' }
  | { type: 'read-error' };

class AccessError extends Error {
  constructor(readonly kind: 'signed-out' | 'unavailable' | 'read-error') {
    super('Creation access unavailable');
  }
}

export function creationProject(
  path: string,
  search: string,
): string | undefined {
  try {
    const parts = path.split('/');
    if (
      search !== '' ||
      parts.length !== 5 ||
      parts[1] !== 'projects' ||
      parts[3] !== 'issues' ||
      parts[4] !== 'new'
    ) {
      return undefined;
    }
    return uuid(parts[2]);
  } catch {
    return undefined;
  }
}

export async function creationAccess(
  project: string,
  expectedActor: string | undefined,
  signal: AbortSignal,
): Promise<string> {
  uuid(project);
  const session = await getCurrentSession(signal);
  if (expectedActor !== undefined && session.user_id !== expectedActor) {
    throw new AccessError('unavailable');
  }
  const response = await fetch(
    `/api/v1/projects/${project}/issues?page_size=1`,
    {
      method: 'HEAD',
      mode: 'same-origin',
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      signal,
    },
  );
  if (response.status !== 200) {
    throw new AccessError(
      response.status === 401
        ? 'signed-out'
        : [403, 404].includes(response.status)
          ? 'unavailable'
          : 'read-error',
    );
  }
  return session.user_id;
}

type Command = { actor: string; key: string; input: IssueCreationInput };
type Dependencies = {
  access: typeof creationAccess;
  send: typeof createIssue;
  key: () => string;
};

export class IssueCreationScreen {
  private current: CreationState = { type: 'idle' };
  private readonly lifetime = new AbortController();
  private operation = new AbortController();
  private generation = 0;
  private actor: string | undefined;
  private command: Command | undefined;
  private confirmed: IssueCreationReceipt | undefined;

  constructor(
    private readonly project: string | undefined,
    private readonly changed: (state: CreationState) => void,
    private readonly deps: Dependencies = {
      access: creationAccess,
      send: createIssue,
      key: () => crypto.randomUUID(),
    },
  ) {}

  get state(): CreationState {
    return this.current;
  }

  private set(state: CreationState): void {
    if (!this.lifetime.signal.aborted) {
      this.current = state;
      this.changed(state);
    }
  }

  suspend(): void {
    if (this.lifetime.signal.aborted) {
      return;
    }
    this.operation.abort();
    this.generation += 1;
    this.set({ type: 'suspended' });
  }

  dispose(): void {
    this.suspend();
    this.lifetime.abort();
    this.command = undefined;
    this.confirmed = undefined;
    this.actor = undefined;
  }

  async start(): Promise<void> {
    if (
      this.lifetime.signal.aborted ||
      ![
        'idle',
        'suspended',
        'read-error',
        'unavailable',
        'signed-out',
      ].includes(this.current.type)
    ) {
      return;
    }
    if (this.project === undefined) {
      this.set({ type: 'unavailable' });
      return;
    }
    this.operation.abort();
    this.operation = new AbortController();
    const generation = ++this.generation;
    const signal = AbortSignal.any([
      this.lifetime.signal,
      this.operation.signal,
      AbortSignal.timeout(8000),
    ]);
    this.set({ type: 'checking' });
    try {
      const actor = await this.deps.access(this.project, this.actor, signal);
      signal.throwIfAborted();
      if (generation !== this.generation || this.lifetime.signal.aborted) {
        return;
      }
      // Check even injected adapters; never transfer an uncertain command.
      if (this.actor !== undefined && actor !== this.actor) {
        throw new AccessError('unavailable');
      }
      this.actor = uuid(actor);
      this.set(
        this.command !== undefined
          ? { type: 'uncertain' }
          : this.confirmed !== undefined
            ? { type: 'confirmed', receipt: this.confirmed }
            : { type: 'editing' },
      );
    } catch (error) {
      if (generation !== this.generation || this.lifetime.signal.aborted) {
        return;
      }
      const type =
        error instanceof AccessError
          ? error.kind
          : error instanceof SessionRequestError && error.status === 401
            ? 'signed-out'
            : error instanceof SessionRequestError &&
                [403, 404].includes(error.status)
              ? 'unavailable'
              : 'read-error';
      this.set({ type });
    }
  }

  async submit(input: IssueCreationInput): Promise<void> {
    if (
      this.current.type !== 'editing' ||
      this.actor === undefined ||
      this.command !== undefined
    ) {
      return;
    }
    serializeIssueCreationInput(input);
    const key = uuid(this.deps.key());
    this.command = {
      actor: this.actor,
      key,
      input: {
        title: input.title,
        ...(input.description === undefined
          ? {}
          : { description: input.description }),
        ...(input.assignee_user_id === undefined
          ? {}
          : { assignee_user_id: input.assignee_user_id }),
      },
    };
    await this.send();
  }

  async retry(): Promise<void> {
    if (this.current.type === 'uncertain' && this.command !== undefined) {
      await this.send();
    }
  }

  private async send(): Promise<void> {
    const command = this.command;
    if (
      command === undefined ||
      this.project === undefined ||
      this.lifetime.signal.aborted
    ) {
      return;
    }
    this.operation.abort();
    this.operation = new AbortController();
    const generation = ++this.generation;
    const signal = AbortSignal.any([
      this.lifetime.signal,
      this.operation.signal,
      AbortSignal.timeout(8000),
    ]);
    this.set({ type: 'submitting' });
    try {
      const result = await this.deps.send(
        this.project,
        command.actor,
        command.key,
        command.input,
        signal,
      );
      signal.throwIfAborted();
      if (generation !== this.generation || this.lifetime.signal.aborted) {
        return;
      }
      this.confirmed = result.receipt;
      this.command = undefined;
      this.set({ type: 'confirmed', receipt: result.receipt });
    } catch {
      if (generation === this.generation && !this.lifetime.signal.aborted) {
        this.set({ type: 'uncertain' });
      }
    }
  }
}
