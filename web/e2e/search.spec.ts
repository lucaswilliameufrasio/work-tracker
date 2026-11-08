import { expect, test, type Route } from '@playwright/test';

const projectId = '019a0000-0000-7000-8000-000000000001';
const statusId = '019a0000-0000-7000-8000-000000000002';
const issueTypeId = '019a0000-0000-7000-8000-000000000003';
const archivedTypeId = '019a0000-0000-7000-8000-000000000099';
const archivedStatusId = '019a0000-0000-7000-8000-000000000097';
const ineligibleAssigneeId = '019a0000-0000-7000-8000-000000000096';
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

async function fulfillMemberCatalog(route: Route): Promise<void> {
  const url = new URL(route.request().url());
  expect(url.searchParams.get('page_size')).toBe('100');
  expect(url.searchParams.get('cursor')).toBeNull();
  await route.fulfill({
    json: {
      data: [
        {
          user_id: actorId,
          display_name: 'Ada',
          role: 'member',
          can_be_assigned: true,
        },
      ],
      has_more: false,
    },
  });
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
    if (path === `/api/v1/projects/${projectId}/statuses`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      expect(url.searchParams.get('page_size')).toBe('100');
      expect(url.searchParams.get('cursor')).toBeNull();
      await route.fulfill({
        json: {
          data: [
            {
              id: statusId,
              project_id: projectId,
              name: 'A fazer',
              category: 'backlog',
              position: '1',
              board_visible: true,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/members`) {
      await fulfillMemberCatalog(route);
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      await route.fulfill({
        json: {
          data: [{ id: issueTypeId, project_id: projectId, name: 'Tarefa' }],
          has_more: false,
        },
      });
      return;
    }
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
    if (path === `/api/v1/projects/${projectId}/statuses`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      expect(url.searchParams.get('page_size')).toBe('100');
      expect(url.searchParams.get('cursor')).toBeNull();
      await route.fulfill({
        json: {
          data: [
            {
              id: statusId,
              project_id: projectId,
              name: 'A fazer',
              category: 'backlog',
              position: '1',
              board_visible: true,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/members`) {
      await fulfillMemberCatalog(route);
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types`) {
      await route.fulfill({
        json: {
          data: [{ id: issueTypeId, project_id: projectId, name: 'Tarefa' }],
          has_more: false,
        },
      });
      return;
    }
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
    if (path === `/api/v1/projects/${projectId}/statuses`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      expect(url.searchParams.get('page_size')).toBe('100');
      expect(url.searchParams.get('cursor')).toBeNull();
      await route.fulfill({
        json: {
          data: [
            {
              id: statusId,
              project_id: projectId,
              name: 'A fazer',
              category: 'backlog',
              position: '1',
              board_visible: true,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/members`) {
      await fulfillMemberCatalog(route);
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types`) {
      await route.fulfill({
        json: {
          data: [{ id: issueTypeId, project_id: projectId, name: 'Tarefa' }],
          has_more: false,
        },
      });
      return;
    }
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

test('filters by an active issue type and keeps only the active archived filter visible', async ({
  page,
}) => {
  const searchRequests: string[] = [];
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    if (path === `/api/v1/projects/${projectId}/statuses`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      expect(url.searchParams.get('page_size')).toBe('100');
      expect(url.searchParams.get('cursor')).toBeNull();
      await route.fulfill({
        json: {
          data: [
            {
              id: statusId,
              project_id: projectId,
              name: 'A fazer',
              category: 'backlog',
              position: '1',
              board_visible: true,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/members`) {
      await fulfillMemberCatalog(route);
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      await route.fulfill({
        json: {
          data: [{ id: issueTypeId, project_id: projectId, name: 'Tarefa' }],
          has_more: false,
        },
      });
      return;
    }
    if (
      path === `/api/v1/projects/${projectId}/issue-types/${archivedTypeId}`
    ) {
      await route.fulfill({
        json: {
          id: archivedTypeId,
          project_id: projectId,
          name: 'Defeito',
          archived_at: '2026-10-24T00:00:00.000000000Z',
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues`) {
      await route.fulfill({ json: { data: [], has_more: false } });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/search`) {
      searchRequests.push(url.search);
      const selectedType = url.searchParams.get('issue_type_id');
      expect(
        selectedType === issueTypeId || selectedType === archivedTypeId,
      ).toBe(true);
      const result = issue(1);
      if (selectedType === archivedTypeId) {
        result.issue_type_id = archivedTypeId;
      }
      await route.fulfill({
        json: { data: [result], has_more: false },
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
  const typeFilter = page.getByLabel('Filtrar por tipo');
  await expect(typeFilter.getByRole('option', { name: 'Tarefa' })).toHaveCount(
    1,
  );
  await expect(
    typeFilter.getByRole('option', { name: /arquivado/ }),
  ).toHaveCount(0);
  await typeFilter.selectOption(issueTypeId);
  await page.getByRole('button', { name: 'Buscar' }).click();
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues?issue_type_id=${issueTypeId}`,
  );
  await expect(
    page.getByRole('link', { name: /DEV-1 Search result 1/ }),
  ).toBeVisible();

  await page.goto(
    `/projects/${projectId}/issues?issue_type_id=${archivedTypeId}`,
  );
  await expect(typeFilter).toHaveValue(archivedTypeId);
  await expect(
    typeFilter.getByRole('option', { name: 'Defeito (arquivado)' }),
  ).toHaveCount(1);
  await expect(typeFilter.getByRole('option', { name: 'Tarefa' })).toHaveCount(
    1,
  );
  expect(searchRequests).toHaveLength(2);
});

test('filters by an active status and keeps only the active archived filter visible', async ({
  page,
}) => {
  const searchRequests: string[] = [];
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    if (path === `/api/v1/projects/${projectId}/statuses`) {
      expect(url.searchParams.get('include_archived')).toBe('false');
      expect(url.searchParams.get('cursor')).toBeNull();
      await route.fulfill({
        json: {
          data: [
            {
              id: statusId,
              project_id: projectId,
              name: 'A fazer',
              category: 'backlog',
              position: '1',
              board_visible: true,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/statuses/${archivedStatusId}`) {
      await route.fulfill({
        json: {
          id: archivedStatusId,
          project_id: projectId,
          name: 'Em progresso',
          category: 'active',
          position: '2',
          board_visible: true,
          archived_at: '2026-10-24T00:00:00.000000000Z',
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/members`) {
      await fulfillMemberCatalog(route);
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types`) {
      await route.fulfill({
        json: {
          data: [{ id: issueTypeId, project_id: projectId, name: 'Tarefa' }],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues`) {
      await route.fulfill({ json: { data: [], has_more: false } });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/search`) {
      searchRequests.push(url.search);
      const selectedStatus = url.searchParams.get('status_id');
      expect(
        selectedStatus === statusId || selectedStatus === archivedStatusId,
      ).toBe(true);
      const result = issue(1);
      if (selectedStatus === archivedStatusId) {
        result.status_id = archivedStatusId;
      }
      await route.fulfill({
        json: { data: [result], has_more: false },
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
  const statusFilter = page.getByLabel('Filtrar por status');
  await expect(
    statusFilter.getByRole('option', { name: 'A fazer' }),
  ).toHaveCount(1);
  await expect(
    statusFilter.getByRole('option', { name: /arquivado/ }),
  ).toHaveCount(0);
  await statusFilter.selectOption(statusId);
  await page.getByRole('button', { name: 'Buscar' }).click();
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues?status_id=${statusId}`,
  );
  await expect(
    page.getByRole('link', { name: /DEV-1 Search result 1/ }),
  ).toBeVisible();

  await page.goto(
    `/projects/${projectId}/issues?status_id=${archivedStatusId}`,
  );
  await expect(statusFilter).toHaveValue(archivedStatusId);
  await expect(
    statusFilter.getByRole('option', { name: 'Em progresso (arquivado)' }),
  ).toHaveCount(1);
  await expect(
    statusFilter.getByRole('option', { name: 'A fazer' }),
  ).toHaveCount(1);
  expect(searchRequests).toHaveLength(2);
});

test('filters by an eligible assignee and keeps only the active ineligible filter visible', async ({
  page,
}) => {
  const searchRequests: string[] = [];
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    if (path === `/api/v1/projects/${projectId}/statuses`) {
      await route.fulfill({
        json: {
          data: [
            {
              id: statusId,
              project_id: projectId,
              name: 'A fazer',
              category: 'backlog',
              position: '1',
              board_visible: true,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (
      path === `/api/v1/projects/${projectId}/members/${ineligibleAssigneeId}`
    ) {
      await route.fulfill({
        json: {
          user_id: ineligibleAssigneeId,
          display_name: 'Nara',
          role: 'viewer',
          can_be_assigned: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/members`) {
      await fulfillMemberCatalog(route);
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issue-types`) {
      await route.fulfill({
        json: {
          data: [{ id: issueTypeId, project_id: projectId, name: 'Tarefa' }],
          has_more: false,
        },
      });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues`) {
      await route.fulfill({ json: { data: [], has_more: false } });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/issues/search`) {
      searchRequests.push(url.search);
      const selectedAssignee = url.searchParams.get('assignee_user_id');
      expect(
        selectedAssignee === actorId ||
          selectedAssignee === ineligibleAssigneeId,
      ).toBe(true);
      const result = issue(1);
      result.assignee_user_id = selectedAssignee;
      await route.fulfill({
        json: { data: [result], has_more: false },
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
  const assigneeFilter = page.getByLabel('Filtrar por responsável');
  await expect(assigneeFilter.getByRole('option', { name: 'Ada' })).toHaveCount(
    1,
  );
  await expect(
    assigneeFilter.getByRole('option', { name: /não elegível/ }),
  ).toHaveCount(0);
  await assigneeFilter.selectOption(actorId);
  await page.getByRole('button', { name: 'Buscar' }).click();
  await expect(page).toHaveURL(
    `/projects/${projectId}/issues?assignee_user_id=${actorId}`,
  );
  await expect(
    page.getByRole('link', { name: /DEV-1 Search result 1/ }),
  ).toBeVisible();

  await page.goto(
    `/projects/${projectId}/issues?assignee_user_id=${ineligibleAssigneeId}`,
  );
  await expect(assigneeFilter).toHaveValue(ineligibleAssigneeId);
  await expect(
    assigneeFilter.getByRole('option', { name: 'Nara (não elegível)' }),
  ).toHaveCount(1);
  await expect(assigneeFilter.getByRole('option', { name: 'Ada' })).toHaveCount(
    1,
  );
  expect(searchRequests).toHaveLength(2);
});
