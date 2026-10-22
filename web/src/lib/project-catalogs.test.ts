import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  getIssueTypePage,
  getStatusPage,
  parseIssueTypePage,
  parseStatusPage,
} from './project-catalogs';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const archived = '2026-10-21T12:00:00.000000000Z';
const common = { id, project_id: id, name: 'A fazer', system_key: 'todo' };
const status = {
  ...common,
  category: 'backlog',
  position: '0',
  board_visible: true,
};
const page = (item: unknown) => ({ data: [item], has_more: false });
const signal = () => new AbortController().signal;
afterEach(() => vi.unstubAllGlobals());

describe('project catalog DTOs', () => {
  it('preserves optional omission, opaque tokens and nonnegative i64 display positions', () => {
    expect(parseStatusPage(page(status), id, {})).toEqual(page(status));
    const maximum = {
      ...status,
      position: '9223372036854775807',
      archived_at: archived,
    };
    expect(
      parseStatusPage(page(maximum), id, { include_archived: true }),
    ).toEqual(page(maximum));
    const type = {
      ...common,
      system_key: 'task',
      name: 'Tarefa',
      icon_key: 'opaque_icon',
      color_key: 'opaque-color',
      archived_at: archived,
    };
    expect(
      parseIssueTypePage(page(type), id, { include_archived: true }),
    ).toEqual(page(type));
    expect(
      parseIssueTypePage(page({ id, project_id: id, name: 'Custom' }), id, {})
        .data[0],
    ).toEqual({ id, project_id: id, name: 'Custom' });
  });

  it('rejects unknown/private fields, wrong project, nulls, malformed IDs, keys and times', () => {
    for (const item of [
      { ...common, token: 'PRIVATE' },
      { ...common, project_id: other },
      { ...common, id: 'bad' },
      { ...common, name: '' },
      { ...common, name: ' ' },
      { ...common, name: '\ud800' },
      { ...common, system_key: null },
      { ...common, system_key: 'Bad Key' },
      { ...common, archived_at: null },
      { ...common, archived_at: '2026-02-31T00:00:00.000000000Z' },
    ]) {
      expect(() =>
        parseIssueTypePage(page(item), id, { include_archived: true }),
      ).toThrow();
      expect(() =>
        parseStatusPage(page({ ...status, ...item }), id, {
          include_archived: true,
        }),
      ).toThrow();
    }
    expect(() =>
      parseIssueTypePage(page({ ...common, icon_key: null }), id, {}),
    ).toThrow();
    expect(() =>
      parseIssueTypePage(
        page({ ...common, color_key: 'x'.repeat(49) }),
        id,
        {},
      ),
    ).toThrow();
    for (const item of [
      { ...status, category: 'unknown' },
      { ...status, position: '01' },
      { ...status, position: '-1' },
      { ...status, position: 1 },
      { ...status, position: '9223372036854775808' },
      { ...status, board_visible: 1 },
    ]) {
      expect(() => parseStatusPage(page(item), id, {})).toThrow();
    }
    expect(() =>
      parseStatusPage(page({ ...status, archived_at: archived }), id, {}),
    ).toThrow();
    expect(() =>
      parseIssueTypePage(page({ ...common, archived_at: archived }), id, {}),
    ).toThrow();
  });

  it('requires UUID ordering, exact progressing full-page cursors and bounded pages', () => {
    for (const parse of [parseStatusPage, parseIssueTypePage]) {
      const item = parse === parseStatusPage ? status : common;
      expect(
        parse({ data: [item], has_more: true, next_cursor: id }, id, {
          page_size: 1,
        }),
      ).toEqual({ data: [item], has_more: true, next_cursor: id });
      expect(
        parse({ data: [], has_more: false }, id, { cursor: id }).data,
      ).toEqual([]);
      for (const raw of [
        { data: [], has_more: true, next_cursor: id },
        { data: [item], has_more: true, next_cursor: other },
        { data: [item], has_more: false, next_cursor: null },
        { data: [item, item], has_more: false },
        { data: [{ ...item, id: other }, item], has_more: false },
      ]) {
        expect(() => parse(raw, id, { page_size: 1 })).toThrow();
      }
      expect(() => parse(page(item), id, { cursor: id })).toThrow();
      expect(() => parse(page(item), id, { page_size: 0 })).toThrow();
      expect(() => parse(page(item), id, { cursor: 'bad' })).toThrow();
    }
    expect(
      parseStatusPage(
        {
          data: [status, { ...status, id: other, position: '0' }],
          has_more: false,
        },
        id,
        { page_size: 2 },
      ).data.length,
    ).toBe(2);
  });
});

describe('catalog GET transport', () => {
  it('validates against the selected request options even if the caller later mutates its object', async () => {
    let complete: ((response: Response) => void) | undefined;
    const fetcher = vi.fn<typeof fetch>().mockImplementation(
      async () =>
        new Promise<Response>((resolve) => {
          complete = resolve;
        }),
    );
    vi.stubGlobal('fetch', fetcher);
    const options = { cursor: id };
    const pending = getIssueTypePage(id, options, signal());
    options.cursor = other;
    complete?.(Response.json(page({ ...common, id: other })));
    expect((await pending).data[0]?.id).toBe(other);
  });
  it('builds only fixed same-origin paths and explicitly selects archive inclusion without traversal', async () => {
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValueOnce(
        Response.json({ data: [status], has_more: true, next_cursor: id }),
      )
      .mockResolvedValueOnce(Response.json({ data: [], has_more: false }));
    vi.stubGlobal('fetch', fetcher);
    await getStatusPage(id, { page_size: 1 }, signal());
    await getIssueTypePage(
      id,
      { cursor: id, include_archived: true },
      signal(),
    );
    expect(fetcher.mock.calls.map(([url]) => url)).toEqual([
      `/api/v1/projects/${id}/statuses?page_size=1&include_archived=false`,
      `/api/v1/projects/${id}/issue-types?page_size=50&include_archived=true&cursor=${id}`,
    ]);
    for (const [, options] of fetcher.mock.calls) {
      expect(options).toMatchObject({
        method: 'GET',
        mode: 'same-origin',
        credentials: 'same-origin',
        redirect: 'error',
        cache: 'no-store',
      });
      expect([...new Headers(options?.headers)]).toEqual([
        ['accept', 'application/json'],
      ]);
      expect(options?.body).toBeUndefined();
    }
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it('rejects local selectors before fetch and keeps safe errors without retries', async () => {
    const fetcher = vi.fn<typeof fetch>();
    vi.stubGlobal('fetch', fetcher);
    await expect(getStatusPage('../private', {}, signal())).rejects.toThrow();
    await expect(
      getIssueTypePage(id, { page_size: 101 }, signal()),
    ).rejects.toThrow();
    await expect(
      getStatusPage(id, { cursor: 'bad' }, signal()),
    ).rejects.toThrow();
    await expect(getStatusPage(id, {}, AbortSignal.abort())).rejects.toThrow();
    expect(fetcher).not.toHaveBeenCalled();
    fetcher.mockResolvedValueOnce(
      Response.json(
        {
          code: 'not_found',
          message: 'PRIVATE',
          request_id: id,
          retryable: false,
          field_errors: {},
        },
        { status: 404 },
      ),
    );
    await expect(getStatusPage(id, {}, signal())).rejects.toMatchObject({
      name: 'IssueRequestError',
      message: 'Issue request failed',
      status: 404,
      code: 'not_found',
    });
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
});
