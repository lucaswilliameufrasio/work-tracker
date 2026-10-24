import { expect, test } from '@playwright/test';

const projectId = '019a0000-0000-7000-8000-000000000001';
const statusId = '019a0000-0000-7000-8000-000000000002';
const issueTypeId = '019a0000-0000-7000-8000-000000000003';
const actorId = '019a0000-0000-7000-8000-000000000004';
const query = 'two literal terms';
const dueDate = '2026-10-30';
const now = '2026-10-24T10:00:00.000000000Z';

function issueId(number: number): string {
  return `019a0000-0000-7000-8000-${String(number).padStart(12, '0')}`;
}

function issue(number: number): Record<string, unknown> {
  return {
    id: issueId(number),
    project_id: projectId,
    issue_number: String(number),
    issue_key: `DEV-${number}`,
    issue_type_id: issueTypeId,
    status_id: statusId,
    title:
      number === 21 ? 'Search result twenty one' : `Search result ${number}`,
    creator_user_id: actorId,
    priority: 'urgent',
    due_date: dueDate,
    is_overdue: false,
    version: '1',
    created_at: now,
    updated_at: now,
  };
}

function boundary(number: number): string {
  return btoa(`${number}/${issueId(number)}`)
    .replaceAll('+', '-')
    .replaceAll('/', '_')
    .replaceAll('=', '');
}

test('project search paginates, opens detail, and returns to the same query and cursor', async ({
  page,
}, testInfo) => {
  const searchRequests: string[] = [];
  const externalRequests: string[] = [];
  const expectedOrigin = new URL(testInfo.project.use.baseURL as string).origin;
  page.on('request', (request) => {
    if (new URL(request.url()).origin !== expectedOrigin) {
      externalRequests.push(request.url());
    }
  });
  await page.route('**/api/v1/**', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (path === `/api/v1/projects/${projectId}/issues/search`) {
      searchRequests.push(url.search);
      expect(url.searchParams.get('q')).toBe(query);
      expect(url.searchParams.get('priority')).toBe('urgent');
      expect(url.searchParams.get('due_date')).toBe(dueDate);
      if (url.searchParams.get('cursor') === null) {
        await route.fulfill({
          json: {
            data: Array.from({ length: 20 }, (_, index) => issue(index + 1)),
            has_more: true,
            next_cursor: boundary(20),
          },
        });
        return;
      }
      expect(url.searchParams.get('cursor')).toBe(boundary(20));
      await route.fulfill({
        json: { data: [issue(21)], has_more: false },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/21`) {
      await route.fulfill({
        json: { ...issue(21), description: 'Private result description' },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/21/history`) {
      await route.fulfill({
        json: {
          data: [
            {
              id: '019a0000-0000-7000-8000-000000000005',
              project_id: projectId,
              issue_id: issueId(21),
              actor_user_id: actorId,
              event_kind: 'issue.created',
              occurred_at: now,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/statuses/${statusId}`) {
      await route.fulfill({
        json: {
          id: statusId,
          project_id: projectId,
          name: 'A fazer',
          category: 'backlog',
          position: '0',
          board_visible: true,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types/${issueTypeId}`) {
      await route.fulfill({
        json: { id: issueTypeId, project_id: projectId, name: 'Tarefa' },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/capabilities`) {
      await route.fulfill({
        json: {
          project_id: projectId,
          can_create_issues: false,
          can_update_issue_fields: false,
          can_replace_issue_text: false,
          can_move_issues: false,
        },
      });
      return;
    }
    if (path === '/api/v1/auth/session') {
      await route.fulfill({
        json: {
          user_id: actorId,
          session_id: actorId,
          csrf_token: 'A'.repeat(43),
        },
      });
      return;
    }
    await route.fulfill({ status: 404, json: { code: 'not_found' } });
  });

  await page.goto(
    `/projects/${projectId}/issues?q=${encodeURIComponent(query)}&priority=urgent&due_date=${dueDate}`,
  );
  await expect(
    page.getByRole('heading', { name: 'Busca de issues' }),
  ).toBeVisible();
  await expect(
    page.getByLabel('Buscar no título, descrição ou chave exata (opcional)'),
  ).toHaveValue(query);
  await expect(page.getByLabel('Filtrar por prioridade')).toHaveValue('urgent');
  await expect(page.getByLabel('Filtrar por data de prazo exata')).toHaveValue(
    dueDate,
  );
  await expect(
    page.getByRole('link', { name: /DEV-1 Search result 1/ }),
  ).toBeVisible();
  await expect(page.getByText('Private result description')).toHaveCount(0);
  const nextCursor = boundary(20);
  const searchPageUrl = new URLSearchParams({
    q: query,
    priority: 'urgent',
    due_date: dueDate,
  });
  searchPageUrl.set('cursor', nextCursor);
  await page
    .getByRole('link', { name: 'Próxima página de resultados' })
    .click();
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues?${searchPageUrl}`,
  );
  await expect(
    page.getByRole('link', { name: /DEV-21 Search result twenty one/ }),
  ).toBeVisible();
  await page
    .getByRole('link', { name: /DEV-21 Search result twenty one/ })
    .click();
  await expect(
    page.getByRole('heading', { name: 'Leitura da issue' }),
  ).toBeVisible();
  await expect(page.getByText('Private result description')).toBeVisible();
  const detailReturn = new URLSearchParams({
    from_q: query,
    from_priority: 'urgent',
    from_due_date: dueDate,
    from_cursor: nextCursor,
  });
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues/21?${detailReturn}`,
  );

  await page
    .getByRole('link', { name: 'Voltar aos resultados da busca' })
    .click();
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues?${searchPageUrl}`,
  );
  await expect(
    page.getByRole('link', { name: /DEV-21 Search result twenty one/ }),
  ).toBeVisible();
  expect(searchRequests).toHaveLength(3);
  expect(
    searchRequests.every(
      (search) =>
        new URLSearchParams(search).get('q') === query &&
        new URLSearchParams(search).get('priority') === 'urgent' &&
        new URLSearchParams(search).get('due_date') === dueDate,
    ),
  ).toBe(true);
  expect(externalRequests).toEqual([]);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
});

test('submits a project search using only a selected priority', async ({
  page,
}) => {
  const searchRequests: string[] = [];
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    if (path === `/api/v1/projects/${projectId}/issues`) {
      await route.fulfill({ json: { data: [], has_more: false } });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/search`) {
      searchRequests.push(url.search);
      expect(url.searchParams.get('q')).toBeNull();
      expect(url.searchParams.get('priority')).toBe('urgent');
      await route.fulfill({
        json: { data: [issue(1)], has_more: false },
      });
      return;
    }
    if (path === '/api/v1/auth/session') {
      await route.fulfill({
        json: {
          user_id: actorId,
          session_id: actorId,
          csrf_token: 'A'.repeat(43),
        },
      });
      return;
    }
    await route.fulfill({ status: 404, json: { code: 'not_found' } });
  });

  await page.goto(`/projects/${projectId}/issues`);
  await page.getByLabel('Filtrar por prioridade').selectOption('urgent');
  await page.getByRole('button', { name: 'Buscar' }).click();
  await expect(page).toHaveURL(`/projects/${projectId}/issues?priority=urgent`);
  await expect(
    page.getByRole('heading', { name: 'Busca de issues' }),
  ).toBeVisible();
  await expect(
    page.getByRole('link', { name: /DEV-1 Search result 1/ }),
  ).toBeVisible();
  expect(searchRequests).toHaveLength(1);
});

test('submits a project search using only an exact due date', async ({
  page,
}) => {
  const searchRequests: string[] = [];
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    if (path === `/api/v1/projects/${projectId}/issues`) {
      await route.fulfill({ json: { data: [], has_more: false } });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/search`) {
      searchRequests.push(url.search);
      expect(url.searchParams.get('q')).toBeNull();
      expect(url.searchParams.get('priority')).toBeNull();
      expect(url.searchParams.get('due_date')).toBe(dueDate);
      await route.fulfill({
        json: { data: [issue(1)], has_more: false },
      });
      return;
    }
    if (path === '/api/v1/auth/session') {
      await route.fulfill({
        json: {
          user_id: actorId,
          session_id: actorId,
          csrf_token: 'A'.repeat(43),
        },
      });
      return;
    }
    await route.fulfill({ status: 404, json: { code: 'not_found' } });
  });

  await page.goto(`/projects/${projectId}/issues`);
  await page.getByLabel('Filtrar por data de prazo exata').fill(dueDate);
  await page.getByRole('button', { name: 'Buscar' }).click();
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues?due_date=${dueDate}`,
  );
  await expect(
    page.getByRole('heading', { name: 'Busca de issues' }),
  ).toBeVisible();
  await expect(
    page.getByRole('link', { name: 'DEV-1 Search result 1' }),
  ).toBeVisible();
  expect(searchRequests).toHaveLength(1);
});
