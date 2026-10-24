import { afterEach, expect, it, vi } from 'vitest';
import {
  parseIssueTextReceipt,
  replaceIssueText,
  serializeIssueTextInput,
} from './issue-text';
import { IssueRequestError } from './issue-transport';
import { onSessionEnded } from './session-lifecycle';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const key = '019a0000-0000-7000-8000-000000000003';
const target = { id: other, issue_number: '9007199254740993' };
const receipt = {
  id: other,
  project_id: id,
  issue_number: target.issue_number,
  issue_key: `DEV-${target.issue_number}`,
  result_version: '2',
  changed: true,
};
const session = { user_id: id, session_id: other, csrf_token: 'A'.repeat(43) };
const input = { title: ' Raw title ', expected_version: '1' };
const signal = () => new AbortController().signal;
const headers = {
  Location: `/api/v1/projects/${id}/issues/${target.issue_number}`,
  'Idempotency-Replayed': 'false',
};
function json(
  value: unknown,
  status = 200,
  extra: Record<string, string> = {},
): Response {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'Content-Type': 'application/json', ...extra },
  });
}
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

it('requires exact target/version arithmetic and strict content-free receipts', () => {
  expect(parseIssueTextReceipt(receipt, id, target, '1')).toEqual(receipt);
  expect(
    parseIssueTextReceipt(
      { ...receipt, result_version: '1', changed: false },
      id,
      target,
      '1',
    ).changed,
  ).toBe(false);
  expect(
    parseIssueTextReceipt(
      { ...receipt, result_version: '9007199254740994' },
      id,
      target,
      '9007199254740993',
    ).result_version,
  ).toBe('9007199254740994');
  expect(
    parseIssueTextReceipt(
      { ...receipt, result_version: '9223372036854775807', changed: false },
      id,
      target,
      '9223372036854775807',
    ).result_version,
  ).toBe('9223372036854775807');
  for (const value of [
    null,
    {},
    { ...receipt, previous_title: 'private' },
    { ...receipt, id },
    { ...receipt, project_id: other },
    { ...receipt, issue_number: '1' },
    { ...receipt, issue_key: 'DEV-2' },
    { ...receipt, result_version: '1' },
    { ...receipt, changed: false },
    { ...receipt, changed: 'true' },
    { ...receipt, result_version: 2 },
    { ...receipt, result_version: '03' },
    { ...receipt, result_version: '9223372036854775808' },
  ]) {
    expect(() => parseIssueTextReceipt(value, id, target, '1')).toThrow();
  }
});

it('rejects invalid input/version/null/arbitrary fields before fetching and preserves omitted clearing', async () => {
  expect(serializeIssueTextInput(input)).toBe(
    '{"title":" Raw title ","expected_version":"1"}',
  );
  for (const value of [
    null,
    {},
    { title: 'a' },
    { ...input, expected_version: 1 },
    { ...input, expected_version: '0' },
    { ...input, expected_version: '-1' },
    { ...input, expected_version: '01' },
    { ...input, expected_version: '9223372036854775808' },
    { ...input, description: null },
    { ...input, assignee_user_id: id },
    { ...input, title: ' ' },
    { ...input, title: '\ud800' },
    { ...input, description: 'é'.repeat(32769) },
  ]) {
    expect(() => serializeIssueTextInput(value)).toThrow();
  }
  const fetcher = vi.fn<typeof fetch>();
  vi.stubGlobal('fetch', fetcher);
  await expect(
    replaceIssueText('bad', target, id, key, input, signal()),
  ).rejects.toThrow();
  await expect(
    replaceIssueText(
      id,
      { ...target, issue_number: '01' },
      id,
      key,
      input,
      signal(),
    ),
  ).rejects.toThrow();
  await expect(
    replaceIssueText(id, target, id, 'bad', input, signal()),
  ).rejects.toThrow();
  expect(fetcher).not.toHaveBeenCalled();
});

it('snapshots target/text/version before CSRF and sends one fixed same-origin PUT', async () => {
  let finish: ((response: Response) => void) | undefined;
  const pending = new Promise<Response>((resolve) => {
    finish = resolve;
  });
  const fetcher = vi
    .fn<typeof fetch>()
    .mockReturnValueOnce(pending)
    .mockResolvedValueOnce(json(receipt, 200, headers));
  vi.stubGlobal('fetch', fetcher);
  const mutableInput = { ...input, description: '' };
  const mutableTarget = { ...target };
  const result = replaceIssueText(
    id,
    mutableTarget,
    id,
    key,
    mutableInput,
    signal(),
  );
  mutableInput.title = 'Changed while waiting';
  mutableInput.expected_version = '99';
  mutableTarget.issue_number = '2';
  finish?.(json(session));
  expect(await result).toEqual({ receipt, replayed: false });
  expect(fetcher.mock.calls[1]?.[0]).toBe(
    `/api/v1/projects/${id}/issues/${target.issue_number}/text`,
  );
  expect(fetcher.mock.calls[1]?.[1]).toMatchObject({
    method: 'PUT',
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
    body: '{"title":" Raw title ","description":"","expected_version":"1"}',
  });
  expect(fetcher).toHaveBeenCalledTimes(2);
});

it('accepts exact explicit replay but never changes a key/version or retries after safe errors', async () => {
  for (const status of [401, 403, 404, 409, 422, 503]) {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(json(session))
      .mockResolvedValueOnce(
        json(
          {
            code: 'issue_version_conflict',
            message: 'PRIVATE ERROR',
            request_id: id,
            field_errors: {},
            retryable: true,
          },
          status,
        ),
      );
    vi.stubGlobal('fetch', fetcher);
    const result = replaceIssueText(id, target, id, key, input, signal());
    await expect(result).rejects.toBeInstanceOf(IssueRequestError);
    await expect(result).rejects.not.toThrow('PRIVATE');
    expect(fetcher).toHaveBeenCalledTimes(2);
  }
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json(receipt, 200, { ...headers, 'Idempotency-Replayed': 'true' }),
    );
  vi.stubGlobal('fetch', fetcher);
  expect(
    (await replaceIssueText(id, target, id, key, input, signal())).replayed,
  ).toBe(true);
  expect(fetcher).toHaveBeenCalledTimes(2);
});

it('does not send for changed identity, aborted input or failed CSRF lookup', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json({ ...session, user_id: other }));
  vi.stubGlobal('fetch', fetcher);
  await expect(
    replaceIssueText(id, target, id, key, input, signal()),
  ).rejects.toThrow('identity changed');
  expect(fetcher).toHaveBeenCalledTimes(1);
  fetcher.mockReset();
  await expect(
    replaceIssueText(id, target, id, key, input, AbortSignal.abort()),
  ).rejects.toThrow();
  expect(fetcher).not.toHaveBeenCalled();
  fetcher.mockRejectedValueOnce(new Error('PRIVATE NETWORK'));
  await expect(
    replaceIssueText(id, target, id, key, input, signal()),
  ).rejects.not.toThrow('PRIVATE');
  expect(fetcher).toHaveBeenCalledTimes(1);
});

it('notifies the session lifecycle when an issue request observes an expired session', async () => {
  let ended = 0;
  const unsubscribe = onSessionEnded(() => {
    ended += 1;
  });
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockResolvedValueOnce(
      json(
        {
          code: 'unauthenticated',
          message: 'safe',
          request_id: id,
          field_errors: {},
          retryable: false,
        },
        401,
      ),
    );
  vi.stubGlobal('fetch', fetcher);
  await expect(
    replaceIssueText(id, target, id, key, input, signal()),
  ).rejects.toBeInstanceOf(IssueRequestError);
  expect(ended).toBe(1);
  unsubscribe();
});

it('treats lost network or malformed success/headers as unconfirmed without resending', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(json(session))
    .mockRejectedValueOnce(new Error('PRIVATE LOST RESPONSE'));
  vi.stubGlobal('fetch', fetcher);
  await expect(
    replaceIssueText(id, target, id, key, input, signal()),
  ).rejects.toThrow('result could not be confirmed');
  expect(fetcher).toHaveBeenCalledTimes(2);
  for (const response of [
    json(receipt, 201, headers),
    json(receipt),
    json(receipt, 200, {
      ...headers,
      Location: 'https://foreign.example.test',
    }),
    json(receipt, 200, { ...headers, 'Idempotency-Replayed': 'yes' }),
    json({ ...receipt, title: 'PRIVATE' }, 200, headers),
    new Response(' '.repeat(4097), {
      headers: { 'Content-Type': 'application/json' },
    }),
  ]) {
    fetcher
      .mockReset()
      .mockResolvedValueOnce(json(session))
      .mockResolvedValueOnce(response);
    await expect(
      replaceIssueText(id, target, id, key, input, signal()),
    ).rejects.toThrow();
    expect(fetcher).toHaveBeenCalledTimes(2);
  }
});

it('honors caller/deadline abort while consuming a stalled PUT response without retry', async () => {
  for (const mode of ['caller', 'deadline']) {
    const caller = new AbortController();
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
            stream.enqueue(new TextEncoder().encode('{'));
            options?.signal?.addEventListener(
              'abort',
              () => stream.error(new DOMException('aborted', 'AbortError')),
              { once: true },
            );
            started?.();
          },
        });
        return new Response(body, {
          headers: { 'Content-Type': 'application/json' },
        });
      });
    vi.stubGlobal('fetch', fetcher);
    const rejected = expect(
      replaceIssueText(id, target, id, key, input, caller.signal),
    ).rejects.toThrow();
    await ready;
    if (mode === 'caller') {
      caller.abort();
    } else {
      deadline.abort(new DOMException('deadline', 'TimeoutError'));
    }
    await rejected;
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(timeout).toHaveBeenCalledWith(8000);
    timeout.mockRestore();
  }
});
