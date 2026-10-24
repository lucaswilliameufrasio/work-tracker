import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  getIssueDetail,
  getIssueHistoryPage,
  getIssuePage,
  IssueRequestError,
  parseIssueDetail,
  parseIssueHistoryPage,
  parseIssuePage,
} from './issues';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const time = '2026-10-21T10:00:00.000000000Z';
const summary = {
  id,
  project_id: id,
  issue_number: '9007199254740993',
  issue_key: 'DEV-9007199254740993',
  issue_type_id: id,
  status_id: id,
  title: 'Untrusted <script>text</script>',
  creator_user_id: id,
  priority: 'none',
  version: '9223372036854775807',
  created_at: time,
  updated_at: time,
};
const event = {
  id,
  project_id: id,
  issue_id: id,
  actor_user_id: id,
  event_kind: 'issue.created',
  occurred_at: time,
};
const cursor = btoa(`${time}/${id}`)
  .replaceAll('+', '-')
  .replaceAll('/', '_')
  .replaceAll('=', '');
const signal = () => new AbortController().signal;
afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('issue DTO boundary', () => {
  it('decodes exact typed text-change events but never text snapshots or unknown kinds', () => {
    for (const kind of [
      'issue.title_updated',
      'issue.description_updated',
      'issue.text_updated',
    ]) {
      const changed = { ...event, event_kind: kind };
      expect(
        parseIssueHistoryPage({ data: [changed], has_more: false }, id, 20)
          .data[0]?.event_kind,
      ).toBe(kind);
      expect(() =>
        parseIssueHistoryPage(
          {
            data: [{ ...changed, previous_title: 'private' }],
            has_more: false,
          },
          id,
          20,
        ),
      ).toThrow();
    }
    expect(() =>
      parseIssueHistoryPage(
        { data: [{ ...event, event_kind: 'issue.anything' }], has_more: false },
        id,
        20,
      ),
    ).toThrow();
  });
  it('accepts only typed priority diffs for structured field events', () => {
    const changed = {
      ...event,
      event_kind: 'issue.field_updated',
      change: { field: 'priority', before: 'none', after: 'urgent' },
    };
    expect(
      parseIssueHistoryPage({ data: [changed], has_more: false }, id, 20)
        .data[0],
    ).toEqual(changed);
    for (const value of [
      {
        ...changed,
        change: { field: 'due_date', before: 'none', after: 'urgent' },
      },
      {
        ...changed,
        change: { field: 'priority', before: 'none', after: 'none' },
      },
      {
        ...changed,
        change: { field: 'priority', before: [], after: 'urgent' },
      },
      { ...changed, title: 'private' },
      { ...event, change: changed.change },
    ]) {
      expect(() =>
        parseIssueHistoryPage({ data: [value], has_more: false }, id, 20),
      ).toThrow();
    }
  });
  it('preserves full positive i64 precision, optional omission and raw text as data', () => {
    expect(parseIssueDetail(summary, id, summary.issue_number)).toEqual(
      summary,
    );
    const detail = {
      ...summary,
      title: '😀'.repeat(240),
      description: 'é'.repeat(32768),
      assignee_user_id: other,
      due_date: '2028-02-29',
      completed_at: time,
      trashed_at: time,
    };
    expect(parseIssueDetail(detail, id, summary.issue_number)).toEqual(detail);
    expect(
      parseIssuePage(
        { data: [summary], has_more: true, next_cursor: summary.issue_number },
        id,
        1,
      ),
    ).toEqual({
      data: [summary],
      has_more: true,
      next_cursor: summary.issue_number,
    });
    expect(
      parseIssuePage({ data: [], has_more: false }, id, 1, summary.issue_number)
        .data,
    ).toEqual([]);
  });

  it('rejects unsafe numbers, mismatched selectors, nulls, unknown fields and corrupt values', () => {
    for (const value of [
      null,
      {},
      { ...summary, token: 'private' },
      { ...summary, history: [] },
      { ...summary, project_id: other },
      { ...summary, issue_number: Number('9007199254740993') },
      { ...summary, issue_number: '01' },
      { ...summary, issue_number: '9223372036854775808' },
      { ...summary, version: '0' },
      { ...summary, id: 'bad' },
      { ...summary, issue_key: 'DEV-2' },
      { ...summary, title: '' },
      { ...summary, title: ' ' },
      { ...summary, title: '😀'.repeat(241) },
      { ...summary, title: '\ud800' },
      { ...summary, description: '\udfff' },
      { ...summary, priority: 'critical' },
      { ...summary, description: null },
      { ...summary, description: 'é'.repeat(32769) },
      { ...summary, assignee_user_id: null },
      { ...summary, due_date: '2026-02-31' },
      { ...summary, completed_at: null },
      { ...summary, updated_at: '2026-02-31T00:00:00.000000000Z' },
    ]) {
      expect(() => parseIssueDetail(value, id, summary.issue_number)).toThrow(
        'Invalid issue API response',
      );
    }
    expect(() => parseIssueDetail(summary, id, '1')).toThrow();
  });

  it('requires bounded ascending pages, unique IDs and exact progressing continuation', () => {
    for (const value of [
      { data: [], has_more: true, next_cursor: '1' },
      { data: [summary], has_more: true, next_cursor: '1' },
      { data: [summary], has_more: false, next_cursor: null },
      { data: [summary, summary], has_more: false },
      {
        data: [{ ...summary, description: 'not a summary field' }],
        has_more: false,
      },
    ]) {
      expect(() => parseIssuePage(value, id, 1)).toThrow();
    }
    expect(() =>
      parseIssuePage(
        { data: [summary], has_more: false },
        id,
        1,
        summary.issue_number,
      ),
    ).toThrow();
    expect(() =>
      parseIssuePage(
        { data: [summary], has_more: true, next_cursor: summary.issue_number },
        id,
        2,
      ),
    ).toThrow();
    expect(() =>
      parseIssuePage({ data: [], has_more: false }, id, 0),
    ).toThrow();
    const earlier = {
      ...summary,
      id: other,
      issue_number: '1',
      issue_key: 'DEV-1',
    };
    expect(
      parseIssuePage({ data: [earlier, summary], has_more: false }, id, 2).data,
    ).toEqual([earlier, summary]);
    expect(() =>
      parseIssuePage({ data: [summary, earlier], has_more: false }, id, 2),
    ).toThrow();
    expect(() =>
      parseIssuePage(
        { data: [{ ...earlier, id }, summary], has_more: false },
        id,
        2,
      ),
    ).toThrow();
  });

  it('validates history cursor encoding, event scope and tuple order without accepting body text', () => {
    expect(
      parseIssueHistoryPage(
        { data: [event], has_more: true, next_cursor: cursor },
        id,
        1,
      ),
    ).toEqual({ data: [event], has_more: true, next_cursor: cursor });
    expect(
      parseIssueHistoryPage({ data: [], has_more: false }, id, 1, cursor).data,
    ).toEqual([]);
    for (const value of [
      { data: [{ ...event, body: 'private' }], has_more: false },
      { data: [{ ...event, project_id: other }], has_more: false },
      { data: [{ ...event, actor_user_id: 'bad' }], has_more: false },
      { data: [{ ...event, event_kind: 'unknown' }], has_more: false },
      { data: [event, event], has_more: false },
      {
        data: [event, { ...event, id: other, issue_id: other }],
        has_more: false,
      },
      { data: [event], has_more: true, next_cursor: cursor + '=' },
      { data: [event], has_more: true, next_cursor: cursor.slice(0, -1) + 'B' },
    ]) {
      expect(() => parseIssueHistoryPage(value, id, 2)).toThrow();
    }
    expect(() =>
      parseIssueHistoryPage({ data: [event], has_more: false }, id, 1, cursor),
    ).toThrow();
    expect(() =>
      parseIssueHistoryPage(
        { data: [], has_more: false },
        id,
        1,
        'x'.repeat(90),
      ),
    ).toThrow();
    const later = { ...event, id: other };
    for (const bad of [
      cursor + '=',
      cursor.slice(0, -1) + 'B',
      btoa(`2026-02-31T00:00:00.000000000Z/${id}`).replaceAll('=', ''),
      btoa(`${time}/gggggggg-gggg-gggg-gggg-gggggggggggg`).replaceAll('=', ''),
    ]) {
      expect(() =>
        parseIssueHistoryPage({ data: [], has_more: false }, id, 1, bad),
      ).toThrow();
      expect(() =>
        parseIssueHistoryPage(
          { data: [event], has_more: true, next_cursor: bad },
          id,
          1,
        ),
      ).toThrow();
    }
    const different = btoa(`${time}/${other}`).replaceAll('=', '');
    expect(() =>
      parseIssueHistoryPage(
        { data: [event], has_more: true, next_cursor: different },
        id,
        1,
      ),
    ).toThrow();
    // Synthetic parser-only shape; current SQLite permits just one creation event.
    expect(
      parseIssueHistoryPage({ data: [event, later], has_more: false }, id, 2)
        .data,
    ).toEqual([event, later]);
  });
});

describe('bounded cookie-only reads', () => {
  it('uses fixed relative paths and validated selectors without automatic pagination', async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        Response.json({
          data: [summary],
          has_more: true,
          next_cursor: summary.issue_number,
        }),
      )
      .mockResolvedValueOnce(Response.json(summary))
      .mockResolvedValueOnce(Response.json({ data: [], has_more: false }));
    vi.stubGlobal('fetch', fetcher);
    await getIssuePage(id, { page_size: 1 }, signal());
    await getIssueDetail(id, summary.issue_number, signal());
    await getIssueHistoryPage(id, '1', { cursor, page_size: 1 }, signal());
    expect(fetcher).toHaveBeenCalledTimes(3);
    expect(fetcher.mock.calls.map(([path]) => path)).toEqual([
      `/api/v1/projects/${id}/issues?page_size=1`,
      `/api/v1/projects/${id}/issues/${summary.issue_number}`,
      `/api/v1/projects/${id}/issues/1/history?page_size=1&cursor=${cursor}`,
    ]);
    for (const [, options] of fetcher.mock.calls) {
      expect(options).toMatchObject({
        method: 'GET',
        mode: 'same-origin',
        credentials: 'same-origin',
        cache: 'no-store',
        redirect: 'error',
      });
      expect(options?.body).toBeUndefined();
      expect([...new Headers(options?.headers)]).toEqual([
        ['accept', 'application/json'],
      ]);
    }
  });

  it('rejects invalid local input and pre-aborted requests before fetch', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    for (const call of [
      () => getIssuePage('../private', {}, signal()),
      () => getIssuePage(id, { page_size: 101 }, signal()),
      () => getIssuePage(id, { cursor: '01' }, signal()),
      () => getIssueDetail(id, '1/../2', signal()),
      () => getIssueHistoryPage(id, '1', { cursor: 'bad' }, signal()),
      () => getIssuePage(id, {}, AbortSignal.abort()),
    ]) {
      await expect(call()).rejects.toThrow();
    }
    expect(fetcher).not.toHaveBeenCalled();
  });

  it('maps error envelopes without reflecting messages, fields or retrying', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    for (const status of [401, 404, 422, 503]) {
      fetcher.mockResolvedValueOnce(
        Response.json(
          {
            code: 'safe_code',
            message: 'PRIVATE',
            request_id: id,
            field_errors: { field: 'PRIVATE' },
            retryable: true,
          },
          { status },
        ),
      );
      await expect(getIssuePage(id, {}, signal())).rejects.toMatchObject({
        name: 'IssueRequestError',
        message: 'Issue request failed',
        status,
        code: 'safe_code',
        request_id: id,
        retryable: true,
      });
    }
    expect(fetcher).toHaveBeenCalledTimes(4);
    expect(
      new IssueRequestError(401, 'unauthenticated', id, false).message,
    ).not.toContain('PRIVATE');
    fetcher.mockRejectedValueOnce(new Error('PRIVATE internals'));
    await expect(getIssuePage(id, {}, signal())).rejects.toThrow(
      'Issue request could not be completed',
    );
  });

  it('rejects malformed content, UTF-8, unknown and inconsistent error envelopes', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    for (const response of [
      new Response('<html>PRIVATE</html>'),
      new Response(undefined, {
        headers: { 'content-type': 'application/json' },
      }),
      new Response('PRIVATE broken JSON', {
        headers: { 'content-type': 'application/json' },
      }),
      new Response(new Uint8Array([0xff]), {
        headers: { 'content-type': 'application/json' },
      }),
      Response.json({ code: 'bad' }, { status: 401 }),
      Response.json(
        {
          code: 'code',
          message: 'PRIVATE',
          field_errors: {},
          request_id: id,
          retryable: false,
        },
        { status: 201 },
      ),
      Response.json(
        {
          code: 'code',
          message: 'PRIVATE',
          field_errors: { field: null },
          request_id: id,
          retryable: false,
        },
        { status: 503 },
      ),
    ]) {
      fetcher.mockResolvedValueOnce(response);
      await expect(getIssuePage(id, {}, signal())).rejects.toThrow(
        'Invalid issue API response',
      );
    }
  });

  it('allows JSON escaping overhead, cancels over-budget streams and bounds error bodies separately', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    const detail = { ...summary, description: '\u0000'.repeat(65536) };
    expect(JSON.stringify(detail).length).toBeGreaterThan(393216);
    fetcher.mockResolvedValueOnce(Response.json(detail));
    expect(await getIssueDetail(id, summary.issue_number, signal())).toEqual(
      detail,
    );
    for (const [status, limit] of [
      [200, 512 * 1024],
      [503, 64 * 1024],
    ]) {
      const cancel = vi.fn();
      const body = new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(new Uint8Array((limit ?? 0) + 1));
        },
        cancel,
      });
      fetcher.mockResolvedValueOnce(
        new Response(body, {
          status,
          headers: { 'content-type': 'application/json' },
        }),
      );
      await expect(getIssuePage(id, {}, signal())).rejects.toThrow(
        'Invalid issue API response',
      );
      expect(cancel).toHaveBeenCalled();
    }
  });

  it('passes caller cancellation to fetch and applies an eight-second deadline without retries', async () => {
    vi.useFakeTimers();
    const timeout = vi.spyOn(AbortSignal, 'timeout');
    const fetcher = vi.fn<typeof fetch>().mockImplementation(
      async (_path, options) =>
        new Promise<Response>((_resolve, reject) => {
          options?.signal?.addEventListener(
            'abort',
            () => reject(new DOMException('aborted', 'AbortError')),
            { once: true },
          );
        }),
    );
    vi.stubGlobal('fetch', fetcher);
    const controller = new AbortController();
    const result = getIssuePage(id, {}, controller.signal);
    const rejected = expect(result).rejects.toThrow();
    controller.abort();
    await rejected;
    expect(timeout).toHaveBeenCalledWith(8000);
    expect(fetcher).toHaveBeenCalledTimes(1);
    timeout.mockRestore();
  });

  it('aborts during streamed body consumption and cancels a stalled read when the deadline fires', async () => {
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
        .mockImplementation(async (_path, options) => {
          const body = new ReadableStream<Uint8Array>({
            start(stream) {
              stream.enqueue(new TextEncoder().encode('{"data":['));
              options?.signal?.addEventListener(
                'abort',
                () => stream.error(new DOMException('aborted', 'AbortError')),
                { once: true },
              );
              started?.();
            },
          });
          return new Response(body, {
            headers: { 'content-type': 'application/json' },
          });
        });
      vi.stubGlobal('fetch', fetcher);
      const result = getIssuePage(id, {}, controller.signal);
      const rejected = expect(result).rejects.toThrow();
      await ready;
      if (mode === 'caller') {
        controller.abort();
      } else {
        deadline.abort(new DOMException('deadline', 'TimeoutError'));
      }
      await rejected;
      expect(timeout).toHaveBeenCalledWith(8000);
      expect(fetcher).toHaveBeenCalledTimes(1);
      timeout.mockRestore();
    }
  });
});
