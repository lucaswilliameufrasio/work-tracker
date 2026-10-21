import {
  getSessionPage,
  revokeAllSessions,
  revokeSession,
  SessionRequestError,
  type SessionPage,
  type SessionSummary,
} from './sessions';

export type TerminationTarget =
  { type: 'all' } | { type: 'session'; session: SessionSummary };

export type SessionScreenState =
  | { type: 'idle' }
  | { type: 'loading' }
  | { type: 'ready'; page: SessionPage; notice?: string }
  | { type: 'confirm'; page: SessionPage; target: TerminationTarget }
  | { type: 'submitting'; target: TerminationTarget }
  | { type: 'signed-out'; reason: 'completed' | 'expired' }
  | { type: 'unavailable' }
  | { type: 'read-error'; notice?: string }
  | { type: 'mutation-uncertain' };

type Client = {
  read: typeof getSessionPage;
  revoke: typeof revokeSession;
  revokeAll: typeof revokeAllSessions;
};

const client: Client = {
  read: getSessionPage,
  revoke: revokeSession,
  revokeAll: revokeAllSessions,
};

export class SessionScreen {
  private current: SessionScreenState = { type: 'idle' };
  private readonly lifetime = new AbortController();
  private readonly changed: (state: SessionScreenState) => void;
  private readonly client: Client;

  constructor(
    changed: (state: SessionScreenState) => void,
    api: Client = client,
  ) {
    this.changed = changed;
    this.client = api;
  }

  get state(): SessionScreenState {
    return this.current;
  }

  dispose(): void {
    this.lifetime.abort();
  }

  async start(): Promise<void> {
    if (this.current.type === 'idle') {
      await this.read();
    }
  }

  async refresh(): Promise<void> {
    if (
      ['ready', 'read-error', 'unavailable', 'mutation-uncertain'].includes(
        this.current.type,
      )
    ) {
      await this.read();
    }
  }

  async next(): Promise<void> {
    if (this.current.type === 'ready' && this.current.page.has_more) {
      await this.read(this.current.page.next_cursor);
    }
  }

  choose(sessionId: string): void {
    if (this.current.type !== 'ready') {
      return;
    }
    const session = this.current.page.data.find(
      (item) => item.id === sessionId,
    );
    if (session !== undefined) {
      this.set({
        type: 'confirm',
        page: this.current.page,
        target: { type: 'session', session },
      });
    }
  }

  chooseAll(): void {
    if (this.current.type === 'ready') {
      this.set({
        type: 'confirm',
        page: this.current.page,
        target: { type: 'all' },
      });
    }
  }

  cancel(): void {
    if (this.current.type === 'confirm') {
      this.set({ type: 'ready', page: this.current.page });
    }
  }

  async submit(): Promise<void> {
    if (this.current.type !== 'confirm') {
      return;
    }
    const target = this.current.target;
    this.set({ type: 'submitting', target });
    try {
      if (target.type === 'all') {
        await this.client.revokeAll(this.lifetime.signal);
      } else {
        await this.client.revoke(target.session.id, this.lifetime.signal);
      }
    } catch (error) {
      if (this.lifetime.signal.aborted) {
        return;
      }
      if (error instanceof SessionRequestError && error.status === 401) {
        this.set({ type: 'signed-out', reason: 'expired' });
      } else if (error instanceof SessionRequestError && error.status === 404) {
        await this.read(
          undefined,
          'A sessão selecionada já não está disponível. Revise a lista.',
        );
      } else {
        this.set({ type: 'mutation-uncertain' });
      }
      return;
    }
    if (this.lifetime.signal.aborted) {
      return;
    }
    if (target.type === 'all' || target.session.is_current) {
      this.set({ type: 'signed-out', reason: 'completed' });
    } else {
      await this.read(undefined, 'A sessão foi encerrada.');
    }
  }

  private set(state: SessionScreenState): void {
    if (!this.lifetime.signal.aborted) {
      this.current = state;
      this.changed(state);
    }
  }

  private async read(cursor?: string, notice?: string): Promise<void> {
    if (this.lifetime.signal.aborted) {
      return;
    }
    this.set({ type: 'loading' });
    try {
      const page = await this.client.read(
        { cursor, page_size: 20 },
        this.lifetime.signal,
      );
      this.set({
        type: 'ready',
        page,
        ...(notice === undefined ? {} : { notice }),
      });
    } catch (error) {
      if (this.lifetime.signal.aborted) {
        return;
      }
      if (error instanceof SessionRequestError && error.status === 401) {
        this.set({ type: 'signed-out', reason: 'expired' });
      } else if (error instanceof SessionRequestError && error.status === 404) {
        this.set({ type: 'unavailable' });
      } else {
        this.set({
          type: 'read-error',
          ...(notice === undefined ? {} : { notice }),
        });
      }
    }
  }
}
