import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  createIssue,
  parseIssueCreationReceipt,
  serializeIssueCreationInput,
} from './issue-creation';
import { IssueRequestError } from './issue-transport';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const key = '019a0000-0000-7000-8000-000000000003';
const receipt = {
  id: other,
  project_id: id,
  issue_number: '9007199254740993',
  issue_key: 'DEV-9007199254740993',
  created_version: '1',
};
const session = { user_id: id, session_id: other, csrf_token: 'A'.repeat(43) };
const signal = () => new AbortController().signal;

function json(
  value: unknown,
  status = 200,
  headers: Record<string, string> = {},
): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'Content-Type': 'application/json', ...headers },
  });
}
function success(replayed = false): Response {
  return json(receipt, 201, {
    Location: `/api/v1/projects/${id}/issues/${receipt.issue_number}`,
    'Idempotency-Replayed': String(replayed),
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('issue creation client', () => {
  it('cancels stalled POST response reads on caller abort or deadline without retry', async () => {
    for (const mode of ['caller', 'deadline']) {
      const controller = new AbortController();
      const deadline = new AbortController();
      const timeout = vi
        .spyOn(AbortSignal, 'timeout')
        .mockReturnValue(deadline.signal);
      let started: (() => void) | undefined;
      const ready = new Promise<void>((resolve) => {
        started = resolve;
      });
      const fetcher = vi
        .fn<typeof fetch>()
        .mockResolvedValueOnce(json(session))
        .mockImplementation(async (_path, options) => {
          const body = new ReadableStream<Uint8Array>({
            start(stream) {
              stream.enqueue(new TextEncoder().encode('{"id":'));
              options?.signal?.addEventListener(
                'abort',
                () => stream.error(new DOMException('aborted', 'AbortError')),
                { once: true },
              );
              started?.();
            },
          });
          return new Response(body, {
            status: 201,
            headers: { 'content-type': 'application/json' },
          });
        });
      vi.stubGlobal('fetch', fetcher);
      const rejected = expect(
        createIssue(id, id, key, { title: 'a' }, controller.signal),
      ).rejects.toThrow();
      await ready;
      if (mode === 'caller') {
        controller.abort();
      } else {
        deadline.abort(new DOMException('deadline', 'TimeoutError'));
      }
      await rejected;
      expect(fetcher).toHaveBeenCalledTimes(2);
      expect(timeout).toHaveBeenCalledWith(8000);
      timeout.mockRestore();
    }
  });
  it('validates strict receipts without losing i64 precision or accepting text snapshots', () => {
    expect(parseIssueCreationReceipt(receipt, id)).toEqual(receipt);
    for (const value of [
      null,
      {},
      { ...receipt, title: 'secret' },
      { ...receipt, id: null },
      { ...receipt, project_id: other },
      { ...receipt, issue_number: 9007199254740992 },
      { ...receipt, issue_number: '01' },
      { ...receipt, issue_number: '0' },
      { ...receipt, issue_number: '9223372036854775808' },
      { ...receipt, issue_key: 'DEV-2' },
      { ...receipt, created_version: 1 },
      { ...receipt, created_version: '2' },
    ]) {
      expect(() => parseIssueCreationReceipt(value, id)).toThrow();
    }
  });

  it('snapshots raw inputs before CSRF fetch then sends one fixed same-origin POST', async () => {
    let resolveSession: (value: Response) => void = () => {
      throw new Error('No pending session request');
    };
    const pending = new Promise<Response>((resolve) => {
      resolveSession = resolve;
    });
    const fetcher = vi
      .fn()
      .mockReturnValueOnce(pending)
      .mockResolvedValueOnce(success());
    vi.stubGlobal('fetch', fetcher);
    const input = {
      title: ' Raw 😀 ',
      description: '',
      assignee_user_id: other,
    };
    const result = createIssue(id, id, key, input, signal());
    input.title = 'changed while waiting';
    input.description = 'changed';
    resolveSession(json(session));
    expect(await result).toEqual({ receipt, replayed: false });
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(fetcher.mock.calls[0]?.[0]).toBe('/api/v1/auth/session');
    expect(fetcher.mock.calls[1]?.[0]).toBe(`/api/v1/projects/${id}/issues`);
    expect(fetcher.mock.calls[1]?.[1]).toMatchObject({
      method: 'POST',
      mode: 'same-origin',
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      headers: {
        Accept: 'application/json',
        'Content-Type': 'application/json',
        'x-csrf-token': session.csrf_token,
        'Idempotency-Key': key,
      },
      body: JSON.stringify({
        title: ' Raw 😀 ',
        description: '',
        assignee_user_id: other,
      }),
    });
    expect(fetcher.mock.calls[1]?.[1].headers).not.toHaveProperty(
      'Authorization',
    );
    expect(fetcher.mock.calls[1]?.[1].headers).not.toHaveProperty('Origin');
  });

  it('omits absent fields and preserves the supplied key across explicit replay calls', async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(json(session))
      .mockResolvedValueOnce(success())
      .mockResolvedValueOnce(json(session))
      .mockResolvedValueOnce(success(true));
    vi.stubGlobal('fetch', fetcher);
    expect(
      (await createIssue(id, id, key, { title: 'a' }, signal())).replayed,
    ).toBe(false);
    expect(
      (await createIssue(id, id, key, { title: 'a' }, signal())).replayed,
    ).toBe(true);
    expect(fetcher).toHaveBeenCalledTimes(4);
    expect(fetcher.mock.calls[1]?.[1].body).toBe('{"title":"a"}');
    expect(fetcher.mock.calls[3]?.[1].headers['Idempotency-Key']).toBe(key);
  });

  it('rejects invalid inputs, nulls, unknown fields and malformed UTF-16 before network', async () => {
    const fetcher = vi.fn();
    vi.stubGlobal('fetch', fetcher);
    for (const title of ['', ' ', '\u0085', '😀'.repeat(241), '\ud800']) {
      await expect(
        createIssue(id, id, key, { title }, signal()),
      ).rejects.toThrow();
    }
    for (const input of [
      { title: 'a', description: 'é'.repeat(32769) },
      { title: 'a', description: '\udfff' },
      { title: 'a', assignee_user_id: 'bad' },
      { title: 'a', actor: id },
    ]) {
      await expect(createIssue(id, id, key, input, signal())).rejects.toThrow();
    }
    // Exercise runtime JS callers without weakening the TypeScript input type.
    expect(() =>
      serializeIssueCreationInput({ title: 'a', description: null }),
    ).toThrow();
    expect(() =>
      serializeIssueCreationInput({ title: 'a', assignee_user_id: null }),
    ).toThrow();
    for (const [project, actor, requestKey] of [
      ['bad', id, key],
      [id, 'bad', key],
      [id, id, 'bad'],
    ]) {
      await expect(
        createIssue(
          project ?? '',
          actor ?? '',
          requestKey ?? '',
          { title: 'a' },
          signal(),
        ),
      ).rejects.toThrow();
    }
    await expect(
      createIssue(id, id, key, { title: ' '.repeat(524288) + 'a' }, signal()),
    ).rejects.toThrow();
    expect(fetcher).not.toHaveBeenCalled();
  });

  it('matches Rust Unicode whitespace/title scalar and UTF-8 description limits', async () => {
    const fetcher = vi
      .fn()
      .mockImplementation(async (_path: string, init: RequestInit) =>
        init.method === 'GET' ? json(session) : success(),
      );
    vi.stubGlobal('fetch', fetcher);
    for (const title of ['😀'.repeat(240), '\u0085a\u0085', '\ufeff']) {
      await createIssue(
        id,
        id,
        key,
        { title, description: '\u0001'.repeat(65536) },
        signal(),
      );
    }
    expect(fetcher).toHaveBeenCalledTimes(6);
  });

  it('does not POST when identity changed or current-session fails', async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(json({ ...session, user_id: other }));
    vi.stubGlobal('fetch', fetcher);
    await expect(
      createIssue(id, id, key, { title: 'private' }, signal()),
    ).rejects.toThrow('identity changed');
    expect(fetcher).toHaveBeenCalledTimes(1);
    fetcher.mockReset().mockRejectedValue(new Error('private network details'));
    await expect(
      createIssue(id, id, key, { title: 'private' }, signal()),
    ).rejects.toThrow('Session request could not be completed');
    expect(fetcher).toHaveBeenCalledTimes(1);
  });

  it('never retries safe errors, ambiguous network failures or malformed successes', async () => {
    for (const status of [401, 403, 404, 409, 415, 422, 503]) {
      const fetcher = vi
        .fn()
        .mockResolvedValueOnce(json(session))
        .mockResolvedValueOnce(
          json(
            {
              code: 'idempotency_conflict',
              message: 'PRIVATE ERROR TEXT',
              request_id: id,
              field_errors: {},
              retryable: true,
            },
            status,
          ),
        );
      vi.stubGlobal('fetch', fetcher);
      const result = createIssue(id, id, key, { title: 'private' }, signal());
      await expect(result).rejects.toBeInstanceOf(IssueRequestError);
      await expect(result).rejects.not.toThrow('PRIVATE');
      expect(fetcher).toHaveBeenCalledTimes(2);
    }
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(json(session))
      .mockRejectedValueOnce(new Error('PRIVATE network details'));
    vi.stubGlobal('fetch', fetcher);
    await expect(
      createIssue(id, id, key, { title: 'private' }, signal()),
    ).rejects.toThrow('result could not be confirmed');
    expect(fetcher).toHaveBeenCalledTimes(2);
    for (const response of [
      json(receipt),
      json(receipt, 201),
      json({ ...receipt, title: 'private' }, 201),
      new Response('not-json', {
        status: 201,
        headers: { 'Content-Type': 'application/json' },
      }),
      json(receipt, 201, {
        Location: 'https://hostile.example.test',
        'Idempotency-Replayed': 'true',
      }),
      json(receipt, 201, {
        Location: `/api/v1/projects/${id}/issues/${receipt.issue_number}`,
        'Idempotency-Replayed': 'yes',
      }),
    ]) {
      fetcher
        .mockReset()
        .mockResolvedValueOnce(json(session))
        .mockResolvedValueOnce(response);
      await expect(
        createIssue(id, id, key, { title: 'private' }, signal()),
      ).rejects.toThrow();
      expect(fetcher).toHaveBeenCalledTimes(2);
    }
  });

  it('bounds response decoding and honors cancellation before POST without retry', async () => {
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce(json(session))
      .mockResolvedValueOnce(
        new Response(' '.repeat(4097), {
          status: 201,
          headers: { 'Content-Type': 'application/json' },
        }),
      );
    vi.stubGlobal('fetch', fetcher);
    await expect(
      createIssue(id, id, key, { title: 'a' }, signal()),
    ).rejects.toThrow();
    expect(fetcher).toHaveBeenCalledTimes(2);
    const controller = new AbortController();
    controller.abort();
    fetcher.mockReset();
    await expect(
      createIssue(id, id, key, { title: 'a' }, controller.signal),
    ).rejects.toThrow();
    expect(fetcher).not.toHaveBeenCalled();
    const another = new AbortController();
    fetcher.mockImplementation(async () => {
      another.abort();
      return json(session);
    });
    await expect(
      createIssue(id, id, key, { title: 'a' }, another.signal),
    ).rejects.toThrow();
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
});
