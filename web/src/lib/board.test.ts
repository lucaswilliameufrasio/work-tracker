import { afterEach, expect, it, vi } from 'vitest';
import { boardBoundary, getBoardPage, parseBoardPage } from './board';
import { loadWork, workRoute } from './issue-navigation';

const id = '019a0000-0000-7000-8000-000000000001';
const other = '019a0000-0000-7000-8000-000000000002';
const time = '2026-10-21T00:00:00.000000000Z';
const card = {
  id,
  project_id: id,
  issue_number: '1',
  issue_key: 'DEV-1',
  issue_type_id: id,
  status_id: id,
  title: 'Untrusted <script>card</script>',
  creator_user_id: id,
  priority: 'none',
  version: '1',
  created_at: time,
  updated_at: time,
  rank: '-9223372036854775808',
};
const cursor = (rank: string, selected = id) =>
  btoa(`${rank}/${selected}`)
    .replaceAll('+', '-')
    .replaceAll('/', '_')
    .replaceAll('=', '');
const signal = () => new AbortController().signal;
afterEach(() => vi.unstubAllGlobals());

it('preserves signed i64 rank extremes and validates numeric rank ordering with UUID ties', () => {
  const middle = { ...card, id: other, rank: '0' };
  const page = {
    data: [card, middle],
    total_count: '3',
    has_more: true,
    next_cursor: cursor('0', other),
  };
  expect(parseBoardPage(page, id, id, { page_size: 2 })).toEqual(page);
  expect(
    parseBoardPage(
      {
        data: [{ ...card, rank: '9223372036854775807' }],
        total_count: '3',
        has_more: false,
      },
      id,
      id,
      { cursor: cursor('0', other) },
    ).data[0]?.rank,
  ).toBe('9223372036854775807');
  expect(
    parseBoardPage({ data: [], total_count: '0', has_more: false }, id, id, {})
      .data,
  ).toEqual([]);
  const ties = [
    { ...card, rank: '0' },
    { ...middle, rank: '0' },
  ];
  expect(
    parseBoardPage(
      { data: ties, total_count: '2', has_more: false },
      id,
      id,
      {},
    ).data,
  ).toEqual(ties);
});

it('rejects private, foreign, unordered, duplicate and inconsistent pages', () => {
  for (const data of [
    [{ ...card, project_id: other }],
    [{ ...card, status_id: other }],
    [{ ...card, description: 'PRIVATE' }],
    [{ ...card, rank: '01' }],
    [{ ...card, rank: '-0' }],
    [{ ...card, rank: '-9223372036854775809' }],
    [{ ...card, rank: '9223372036854775808' }],
    [card, card],
    [
      { ...card, rank: '1' },
      { ...card, id: other, rank: '0' },
    ],
  ]) {
    expect(() =>
      parseBoardPage({ data, total_count: '4', has_more: false }, id, id, {}),
    ).toThrow();
  }
  for (const raw of [
    { data: [card], total_count: 1, has_more: false },
    { data: [card], total_count: '0', has_more: false },
    {
      data: [card],
      total_count: '1',
      has_more: true,
      next_cursor: cursor(card.rank),
    },
    { data: [], total_count: '2', has_more: true, next_cursor: cursor('0') },
    {
      data: [card],
      total_count: '2',
      has_more: true,
      next_cursor: cursor('0'),
    },
    { data: [card], total_count: '2', has_more: false, next_cursor: null },
  ]) {
    expect(() => parseBoardPage(raw, id, id, { page_size: 1 })).toThrow();
  }
  expect(() =>
    parseBoardPage(
      { data: [card], total_count: '1', has_more: false },
      id,
      id,
      { cursor: cursor(card.rank) },
    ),
  ).toThrow();
});

it('validates opaque boundaries canonically rather than trusting Base64 syntax', () => {
  expect(boardBoundary(cursor('0'))).toEqual({ rank: '0', id });
  for (const raw of [
    '01',
    '+1',
    '-0',
    '9223372036854775808',
    '-9223372036854775809',
  ]) {
    expect(() => boardBoundary(cursor(raw))).toThrow();
  }
  expect(() => boardBoundary(cursor('0') + '=')).toThrow();
  expect(() => boardBoundary('x'.repeat(70))).toThrow();
  expect(() => boardBoundary(cursor('0', 'BAD'))).toThrow();
});

it('builds fixed cookie-only GET and does not retry or traverse next pages', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValueOnce(
      Response.json({
        data: [card],
        total_count: '2',
        has_more: true,
        next_cursor: cursor(card.rank),
      }),
    )
    .mockResolvedValueOnce(
      Response.json(
        {
          code: 'not_found',
          message: 'PRIVATE',
          field_errors: {},
          request_id: id,
          retryable: false,
        },
        { status: 404 },
      ),
    );
  vi.stubGlobal('fetch', fetcher);
  await getBoardPage(id, id, { page_size: 1 }, signal());
  expect(fetcher).toHaveBeenCalledTimes(1);
  expect(fetcher.mock.calls[0]?.[0]).toBe(
    `/api/v1/projects/${id}/board/${id}?page_size=1`,
  );
  expect(fetcher.mock.calls[0]?.[1]).toMatchObject({
    method: 'GET',
    mode: 'same-origin',
    credentials: 'same-origin',
    cache: 'no-store',
    redirect: 'error',
  });
  await expect(getBoardPage(id, id, {}, signal())).rejects.toMatchObject({
    status: 404,
    message: 'Issue request failed',
  });
  await expect(getBoardPage(id, '../private', {}, signal())).rejects.toThrow();
  await expect(
    getBoardPage(id, id, { cursor: 'bad' }, signal()),
  ).rejects.toThrow();
  await expect(
    getBoardPage(id, id, { page_size: 101 }, signal()),
  ).rejects.toThrow();
  expect(fetcher).toHaveBeenCalledTimes(2);
});

it('validates overview and focused column routes with independent cursor grammars', () => {
  expect(workRoute(`/projects/${id}/board`, `?status_cursor=${other}`)).toEqual(
    { type: 'board', projectId: id, statusCursor: other },
  );
  expect(
    workRoute(`/projects/${id}/board/${other}`, `?cursor=${cursor('0')}`),
  ).toEqual({
    type: 'column',
    projectId: id,
    statusId: other,
    cursor: cursor('0'),
  });
  for (const [path, search] of [
    [`/projects/${id}/board`, '?cursor=1'],
    [`/projects/${id}/board/bad`, ''],
    [`/projects/${id}/board/${other}`, '?cursor=bad'],
  ]) {
    expect(workRoute(path ?? '', search ?? '')).toEqual({ type: 'invalid' });
  }
});

it('composes only visible status candidates and orders that bounded page by numeric position', async () => {
  const third = '019a0000-0000-7000-8000-000000000003';
  const fourth = '019a0000-0000-7000-8000-000000000004';
  const statuses = [id, other, third, fourth].map((selected, index) => ({
    id: selected,
    project_id: id,
    name: `Status ${index}`,
    category: 'backlog',
    position: String(3 - index),
    board_visible: index === 1 || index === 2,
  }));
  const fetcher = vi.fn<typeof fetch>().mockImplementation(async (path) => {
    if (typeof path !== 'string') {
      throw new Error('Expected relative URL');
    }
    if (path.includes('/statuses?')) {
      return Response.json({ data: statuses, has_more: false });
    }
    return Response.json({ data: [], total_count: '0', has_more: false });
  });
  vi.stubGlobal('fetch', fetcher);
  const data = await loadWork({ type: 'board', projectId: id }, signal());
  expect(data.type).toBe('board');
  if (data.type !== 'board') {
    throw new Error('Expected board data');
  }
  expect(data.columns.map((column) => column.status.id)).toEqual([
    third,
    other,
  ]);
  expect(fetcher).toHaveBeenCalledTimes(3);
  expect(fetcher.mock.calls[0]?.[0]).toContain('page_size=4');
});
