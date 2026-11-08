import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  getCurrentSession,
  getSessionPage,
  logoutCurrentSession,
  parseCurrentSession,
  parseSessionPage,
  revokeAllSessions,
  revokeSession,
  SessionRequestError,
} from './sessions';
import { onSessionEnded } from './session-lifecycle';

const id = '019a0000-0000-7000-8000-000000000001';
const older = '019a0000-0000-7000-8000-000000000000';
const current = { user_id: id, session_id: id, csrf_token: 'A'.repeat(43) };
const summary = {
  id,
  created_at: '2026-10-19T10:00:00.000000000Z',
  last_activity_at: '2026-10-19T10:01:00.000000000Z',
  expires_at: '2026-10-26T10:00:00.000000000Z',
  is_current: true,
};
const signal = () => new AbortController().signal;

afterEach(() => vi.unstubAllGlobals());

describe('session response boundary', () => {
  it('accepts only safe current-session fields and canonical CSRF encoding', () => {
    expect(parseCurrentSession(current)).toEqual(current);
    for (const value of [
      null,
      {},
      { ...current, token_hash: 'must not pass' },
      { ...current, user_id: 'bad' },
      { ...current, csrf_token: 'B'.repeat(43) },
    ]) {
      expect(() => parseCurrentSession(value)).toThrow(
        'Invalid session API response',
      );
    }
  });

  it('accepts empty/short candidate pages and omits absent cursors', () => {
    expect(
      parseSessionPage({ data: [], has_more: true, next_cursor: older }, 1, id),
    ).toEqual({ data: [], has_more: true, next_cursor: older });
    expect(parseSessionPage({ data: [summary], has_more: false }, 1)).toEqual({
      data: [summary],
      has_more: false,
    });
    expect(
      parseSessionPage(
        { data: [summary], has_more: true, next_cursor: older },
        2,
      ),
    ).toEqual({ data: [summary], has_more: true, next_cursor: older });
  });

  it('rejects malformed, unordered, oversized, non-progressing and private data', () => {
    for (const value of [
      { data: [], has_more: true },
      { data: [], has_more: false, next_cursor: null },
      { data: [summary, summary], has_more: false },
      { data: [{ ...summary, id: older }, summary], has_more: false },
      { data: [{ ...summary, token: 'not allowed' }], has_more: false },
      {
        data: [{ ...summary, created_at: '2026-02-31T10:00:00.000000000Z' }],
        has_more: false,
      },
      {
        data: [{ ...summary, expires_at: summary.created_at }],
        has_more: false,
      },
      { data: [{ ...summary, is_current: null }], has_more: false },
      { data: [summary, { ...summary, id: older }], has_more: false },
    ]) {
      expect(() => parseSessionPage(value, 2)).toThrow(
        'Invalid session API response',
      );
    }
    expect(() =>
      parseSessionPage({ data: [], has_more: true, next_cursor: id }, 2, id),
    ).toThrow();
    expect(() =>
      parseSessionPage({ data: [summary], has_more: false }, 0),
    ).toThrow();
    expect(() =>
      parseSessionPage(
        { data: [summary, { ...summary, id: older }], has_more: false },
        1,
      ),
    ).toThrow();
  });
});

describe('same-origin session transport', () => {
  it('uses bounded explicit pages without bearer/storage access or automatic traversal', async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValue(
        Response.json({ data: [], has_more: true, next_cursor: older }),
      );
    vi.stubGlobal('fetch', fetcher);
    const result = await getSessionPage({ cursor: id, page_size: 1 }, signal());
    expect(result.next_cursor).toBe(older);
    expect(fetcher).toHaveBeenCalledTimes(1);
    const [path, options] = fetcher.mock.calls[0] ?? [];
    expect(path).toBe(`/api/v1/auth/sessions?page_size=1&cursor=${id}`);
    expect(options).toMatchObject({
      method: 'GET',
      credentials: 'same-origin',
      mode: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
    });
    const headers = new Headers(options?.headers);
    expect(headers.has('cookie')).toBe(false);
    expect(headers.has('authorization')).toBe(false);
    expect(headers.has('origin')).toBe(false);
    expect(options?.body).toBeUndefined();
  });

  it('rejects bad local input and pre-aborted calls before fetch', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    await expect(
      getSessionPage({ page_size: 101 }, signal()),
    ).rejects.toThrow();
    await expect(getSessionPage({ cursor: 'bad' }, signal())).rejects.toThrow();
    await expect(revokeSession('bad', signal())).rejects.toThrow();
    const controller = new AbortController();
    controller.abort();
    await expect(getCurrentSession(controller.signal)).rejects.toThrow();
    expect(fetcher).not.toHaveBeenCalled();
  });

  it('fetches fresh CSRF for every explicit termination and never adds a body', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    for (const command of [
      () => revokeSession(id, signal()),
      () => revokeAllSessions(signal()),
      () => logoutCurrentSession(signal()),
    ]) {
      fetcher
        .mockResolvedValueOnce(Response.json(current))
        .mockResolvedValueOnce(new Response(undefined, { status: 204 }));
      await command();
    }
    expect(fetcher).toHaveBeenCalledTimes(6);
    const paths = fetcher.mock.calls.map(([path]) => path);
    expect(paths).toEqual([
      '/api/v1/auth/session',
      `/api/v1/auth/sessions/${id}`,
      '/api/v1/auth/session',
      '/api/v1/auth/sessions',
      '/api/v1/auth/session',
      '/api/v1/auth/session',
    ]);
    for (const [, options] of fetcher.mock.calls.filter(
      ([, options]) => options?.method === 'DELETE',
    )) {
      expect(new Headers(options?.headers).get('x-csrf-token')).toBe(
        current.csrf_token,
      );
      expect(new Headers(options?.headers).has('origin')).toBe(false);
      expect(options?.body).toBeUndefined();
    }
  });

  it('announces only confirmed current/global session termination and observed expiry', async () => {
    let ended = 0;
    const unsubscribe = onSessionEnded(() => {
      ended += 1;
    });
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    fetcher.mockResolvedValueOnce(
      Response.json(
        {
          code: 'unauthenticated',
          message: 'safe',
          request_id: id,
          field_errors: {},
          retryable: false,
        },
        { status: 401 },
      ),
    );
    await expect(getCurrentSession(signal())).rejects.toBeInstanceOf(
      SessionRequestError,
    );
    expect(ended).toBe(1);
    fetcher.mockResolvedValueOnce(
      Response.json(
        {
          code: 'unauthenticated',
          message: 'safe',
          request_id: id,
          field_errors: {},
          retryable: false,
        },
        { status: 401 },
      ),
    );
    await expect(getSessionPage({}, signal())).rejects.toBeInstanceOf(
      SessionRequestError,
    );
    expect(ended).toBe(2);
    for (const command of [
      () => revokeSession(id, signal()),
      () => revokeAllSessions(signal()),
      () => logoutCurrentSession(signal()),
    ]) {
      fetcher
        .mockResolvedValueOnce(Response.json(current))
        .mockResolvedValueOnce(new Response(undefined, { status: 204 }));
      await command();
    }
    expect(ended).toBe(5);
    fetcher
      .mockResolvedValueOnce(Response.json(current))
      .mockResolvedValueOnce(new Response(undefined, { status: 204 }));
    await revokeSession(older, signal());
    expect(ended).toBe(5);
    unsubscribe();
  });

  it('keeps safe errors but never raw messages or automatic mutation retries', async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(Response.json(current))
      .mockResolvedValueOnce(
        Response.json(
          {
            code: 'service_busy',
            message: 'do not expose this message',
            request_id: id,
            field_errors: {},
            retryable: true,
          },
          { status: 503 },
        ),
      );
    vi.stubGlobal('fetch', fetcher);
    await expect(revokeAllSessions(signal())).rejects.toMatchObject({
      name: 'SessionRequestError',
      status: 503,
      code: 'service_busy',
      request_id: id,
      retryable: true,
      message: 'Session request failed',
    });
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(
      new SessionRequestError(401, 'unauthenticated', id, false).message,
    ).toBe('Session request failed');
    fetcher
      .mockResolvedValueOnce(Response.json(current))
      .mockRejectedValueOnce(new Error('raw network internals'));
    await expect(revokeAllSessions(signal())).rejects.toThrow(
      'Session request could not be completed',
    );
    expect(fetcher).toHaveBeenCalledTimes(4);
  });

  it('refuses HTML, malformed JSON, oversized bodies and inconsistent error envelopes', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    for (const response of [
      new Response('<html>not API</html>'),
      new Response('private broken JSON', {
        headers: { 'content-type': 'application/json' },
      }),
      new Response('x'.repeat(65537), {
        headers: { 'content-type': 'application/json' },
      }),
      Response.json({ code: 'unauthenticated' }, { status: 401 }),
    ]) {
      fetcher.mockResolvedValueOnce(response);
      await expect(getCurrentSession(signal())).rejects.toThrow(
        'Invalid session API response',
      );
    }
    expect(fetcher).toHaveBeenCalledTimes(4);
  });

  it('propagates caller cancellation into the real fetch signal', async () => {
    const controller = new AbortController();
    let entered: (() => void) | undefined;
    const ready = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const fetcher = vi
      .fn<typeof fetch>()
      .mockImplementation(async (_path, options) => {
        entered?.();
        return new Promise<Response>((_resolve, reject) => {
          options?.signal?.addEventListener(
            'abort',
            () => reject(new Error('aborted')),
            { once: true },
          );
        });
      });
    vi.stubGlobal('fetch', fetcher);
    const request = getCurrentSession(controller.signal);
    const rejected = expect(request).rejects.toThrow();
    await ready;
    controller.abort();
    await rejected;
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('accepts the exact response byte budget and cancels overflow streams', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    const valid = JSON.stringify(current);
    fetcher.mockResolvedValueOnce(
      new Response(valid + ' '.repeat(65536 - valid.length), {
        headers: { 'content-type': 'application/json' },
      }),
    );
    expect(await getCurrentSession(signal())).toEqual(current);
    let cancelled = false;
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new Uint8Array(65536));
        controller.enqueue(new Uint8Array(1));
      },
      cancel() {
        cancelled = true;
      },
    });
    fetcher.mockResolvedValueOnce(
      new Response(stream, { headers: { 'content-type': 'application/json' } }),
    );
    await expect(getCurrentSession(signal())).rejects.toThrow(
      'Invalid session API response',
    );
    expect(cancelled).toBe(true);
  });

  it('never submits a termination without a valid fresh current-session response', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    fetcher.mockResolvedValueOnce(
      Response.json(
        {
          code: 'unauthenticated',
          message: 'not echoed',
          request_id: id,
          field_errors: {},
          retryable: false,
        },
        { status: 401 },
      ),
    );
    await expect(revokeAllSessions(signal())).rejects.toBeInstanceOf(
      SessionRequestError,
    );
    expect(fetcher).toHaveBeenCalledTimes(1);
    fetcher.mockResolvedValueOnce(
      Response.json({ ...current, csrf_token: 'invalid' }),
    );
    await expect(revokeSession(id, signal())).rejects.toThrow(
      'Invalid session API response',
    );
    expect(fetcher).toHaveBeenCalledTimes(2);
    for (const [, options] of fetcher.mock.calls) {
      expect(options?.method).toBe('GET');
    }
  });
});
