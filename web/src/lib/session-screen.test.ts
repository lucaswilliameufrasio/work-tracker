import { describe, expect, it, vi } from 'vitest';
import { SessionScreen, type SessionScreenState } from './session-screen';
import {
  getSessionPage,
  revokeAllSessions,
  revokeSession,
  SessionRequestError,
  type SessionPage,
} from './sessions';

const id = '019a0000-0000-7000-8000-000000000002';
const other = '019a0000-0000-7000-8000-000000000001';
const page: SessionPage = {
  data: [id, other].map((id, index) => ({
    id,
    is_current: index === 0,
    created_at: '2026-10-21T00:00:00.000000000Z',
    last_activity_at: '2026-10-21T00:00:00.000000000Z',
    expires_at: '2026-10-28T00:00:00.000000000Z',
  })),
  has_more: false,
};

function fixture() {
  const api = {
    read: vi.fn<typeof getSessionPage>().mockResolvedValue(page),
    revoke: vi.fn<typeof revokeSession>().mockResolvedValue(undefined),
    revokeAll: vi.fn<typeof revokeAllSessions>().mockResolvedValue(undefined),
  };
  const changes: SessionScreenState[] = [];
  const screen = new SessionScreen((state) => changes.push(state), api);
  return { screen, api, changes };
}

describe('single-state session workflow', () => {
  it('loads only one bounded page and does not skip empty continuation pages', async () => {
    const { screen, api } = fixture();
    api.read.mockResolvedValueOnce({
      data: [],
      has_more: true,
      next_cursor: id,
    });
    await screen.start();
    expect(screen.state).toMatchObject({
      type: 'ready',
      page: { data: [], has_more: true },
    });
    expect(api.read).toHaveBeenCalledTimes(1);
    await screen.next();
    expect(api.read.mock.calls[1]?.[0]).toEqual({ cursor: id, page_size: 20 });
    await screen.next();
    expect(api.read).toHaveBeenCalledTimes(2);
    await screen.refresh();
    expect(api.read.mock.calls[2]?.[0]).toEqual({
      cursor: undefined,
      page_size: 20,
    });
  });

  it('requires explicit selection and confirmation, and cancel never mutates', async () => {
    const { screen, api } = fixture();
    await screen.start();
    await screen.submit();
    screen.choose('not-on-this-page');
    expect(screen.state.type).toBe('ready');
    screen.choose(other);
    expect(screen.state).toMatchObject({
      type: 'confirm',
      target: { type: 'session', session: { id: other } },
    });
    screen.chooseAll();
    expect(screen.state).toMatchObject({
      type: 'confirm',
      target: { type: 'session' },
    });
    screen.cancel();
    expect(screen.state.type).toBe('ready');
    expect(api.revoke).not.toHaveBeenCalled();
    expect(api.revokeAll).not.toHaveBeenCalled();
  });

  it('rereads after another-session success without retaining old private rows', async () => {
    const { screen, api } = fixture();
    await screen.start();
    screen.choose(other);
    await screen.submit();
    expect(api.revoke.mock.calls[0]?.[0]).toBe(other);
    expect(api.read).toHaveBeenCalledTimes(2);
    expect(screen.state).toMatchObject({
      type: 'ready',
      notice: 'A sessão foi encerrada.',
    });
  });

  it('clears private state after current or global success', async () => {
    for (const target of ['current', 'all']) {
      const { screen, api } = fixture();
      await screen.start();
      if (target === 'all') {
        screen.chooseAll();
      } else {
        screen.choose(id);
      }
      await screen.submit();
      expect(screen.state).toEqual({ type: 'signed-out', reason: 'completed' });
      expect(api.read).toHaveBeenCalledTimes(1);
    }
  });

  it('blocks double submission and other controls during a pending command', async () => {
    const { screen, api } = fixture();
    let finish: (() => void) | undefined;
    api.revokeAll.mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve;
        }),
    );
    await screen.start();
    screen.chooseAll();
    const pending = screen.submit();
    expect(screen.state.type).toBe('submitting');
    await screen.submit();
    await screen.refresh();
    await screen.next();
    screen.cancel();
    screen.choose(other);
    expect(api.revokeAll).toHaveBeenCalledTimes(1);
    expect(api.read).toHaveBeenCalledTimes(1);
    finish?.();
    await pending;
    expect(screen.state.type).toBe('signed-out');
  });

  it('never retries an unconfirmed mutation; the recovery action is a read', async () => {
    const { screen, api } = fixture();
    api.revoke.mockRejectedValue(new Error('network response lost'));
    await screen.start();
    screen.choose(other);
    await screen.submit();
    expect(screen.state).toEqual({ type: 'mutation-uncertain' });
    await screen.submit();
    expect(api.revoke).toHaveBeenCalledTimes(1);
    await screen.refresh();
    expect(api.read).toHaveBeenCalledTimes(2);
    expect(api.revoke).toHaveBeenCalledTimes(1);
  });

  it('keeps confirmed completion separate from a subsequent read failure', async () => {
    const { screen, api } = fixture();
    await screen.start();
    api.read.mockRejectedValueOnce(new Error('read unavailable'));
    screen.choose(other);
    await screen.submit();
    expect(screen.state).toEqual({
      type: 'read-error',
      notice: 'A sessão foi encerrada.',
    });
    expect(api.revoke).toHaveBeenCalledTimes(1);
  });

  it('maps read401/404 to closed states and target404 to a safe reread', async () => {
    for (const status of [401, 404, 503]) {
      const { screen, api } = fixture();
      api.read.mockRejectedValueOnce(
        new SessionRequestError(status, 'test_error', id, false),
      );
      await screen.start();
      expect(screen.state.type).toBe(
        status === 401
          ? 'signed-out'
          : status === 404
            ? 'unavailable'
            : 'read-error',
      );
    }
    const { screen, api } = fixture();
    await screen.start();
    api.revoke.mockRejectedValueOnce(
      new SessionRequestError(404, 'not_found', id, false),
    );
    screen.choose(other);
    await screen.submit();
    expect(screen.state.type).toBe('ready');
    expect(api.read).toHaveBeenCalledTimes(2);
    screen.choose(other);
    api.revoke.mockRejectedValueOnce(
      new SessionRequestError(401, 'unauthenticated', id, false),
    );
    await screen.submit();
    expect(screen.state).toEqual({ type: 'signed-out', reason: 'expired' });
  });

  it('aborts and ignores late read and mutation completions after disposal', async () => {
    const { screen, api, changes } = fixture();
    let resolveRead: ((page: SessionPage) => void) | undefined;
    api.read.mockImplementationOnce(
      () =>
        new Promise<SessionPage>((resolve) => {
          resolveRead = resolve;
        }),
    );
    const pending = screen.start();
    screen.dispose();
    expect(api.read.mock.calls[0]?.[1].aborted).toBe(true);
    resolveRead?.(page);
    await pending;
    expect(changes).toEqual([{ type: 'loading' }]);
    const second = fixture();
    await second.screen.start();
    let resolveMutation: (() => void) | undefined;
    second.api.revokeAll.mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          resolveMutation = resolve;
        }),
    );
    second.screen.chooseAll();
    const mutation = second.screen.submit();
    second.screen.dispose();
    resolveMutation?.();
    await mutation;
    expect(second.screen.state.type).toBe('submitting');
    expect(second.api.read).toHaveBeenCalledTimes(1);
  });
});
