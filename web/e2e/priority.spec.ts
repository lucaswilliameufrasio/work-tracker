import { expect, test } from '@playwright/test';

const projectId = '019a0000-0000-7000-8000-000000000001';
const issueId = '019a0000-0000-7000-8000-000000000002';
const statusId = '019a0000-0000-7000-8000-000000000003';
const issueTypeId = '019a0000-0000-7000-8000-000000000004';
const actorId = '019a0000-0000-7000-8000-000000000005';
const creationEventId = '019a0000-0000-7000-8000-000000000006';
const priorityEventId = '019a0000-0000-7000-8000-000000000007';
const now = '2026-10-21T10:00:00.000000000Z';
const changedAt = '2026-10-21T10:01:00.000000000Z';

test('issue detail updates priority through the versioned one-field command', async ({
  page,
}) => {
  let priority = 'none';
  let version = '1';
  let changed = false;
  let mutationCount = 0;
  let mutationKey = '';
  const issue = () => ({
    id: issueId,
    project_id: projectId,
    issue_number: '1',
    issue_key: 'DEV-1',
    issue_type_id: issueTypeId,
    status_id: statusId,
    title: 'Priority fixture',
    description: 'Private text stays outside history.',
    creator_user_id: actorId,
    priority,
    is_overdue: false,
    version,
    created_at: now,
    updated_at: changed ? changedAt : now,
  });
  const events = () => {
    const result: Array<Record<string, unknown>> = [
      {
        id: creationEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.created',
        occurred_at: now,
      },
    ];
    if (changed) {
      result.push({
        id: priorityEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.field_updated',
        change: { field: 'priority', before: 'none', after: 'high' },
        occurred_at: changedAt,
      });
    }
    return result;
  };

  await page.route('**/api/v1/**', async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (
      path === `/api/v1/projects/${projectId}/issues/1` &&
      request.method() === 'GET'
    ) {
      await route.fulfill({ json: issue() });
      return;
    }
    if (
      path === `/api/v1/projects/${projectId}/issues/1/history` &&
      request.method() === 'GET'
    ) {
      await route.fulfill({ json: { data: events(), has_more: false } });
      return;
    }
    if (path === `/api/v1/projects/${projectId}/statuses/${statusId}`) {
      await route.fulfill({
        json: {
          id: statusId,
          project_id: projectId,
          name: 'Em andamento',
          category: 'active',
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
          can_create_issues: true,
          can_update_issue_fields: true,
          can_replace_issue_text: true,
          can_move_issues: true,
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
    if (
      path === `/api/v1/projects/${projectId}/issues/1/priority` &&
      request.method() === 'PUT'
    ) {
      mutationCount += 1;
      mutationKey = request.headers()['idempotency-key'] ?? '';
      expect(request.headers()['x-csrf-token']).toBe('A'.repeat(43));
      expect(request.headers().origin).toBe(url.origin);
      const body = request.postDataJSON() as {
        expected_version: string;
        priority: string;
      };
      expect(Object.keys(body).sort()).toEqual([
        'expected_version',
        'priority',
      ]);
      expect(body).toEqual({ expected_version: '1', priority: 'high' });
      priority = 'high';
      version = '2';
      changed = true;
      await route.fulfill({
        status: 200,
        headers: {
          'cache-control': 'no-store',
          location: `/api/v1/projects/${projectId}/issues/1`,
          'idempotency-replayed': 'false',
        },
        json: {
          id: issueId,
          project_id: projectId,
          issue_number: '1',
          issue_key: 'DEV-1',
          priority: 'high',
          result_version: '2',
          changed: true,
        },
      });
      return;
    }
    await route.fulfill({ status: 404, json: { code: 'not_found' } });
  });

  await page.goto(`/projects/${projectId}/issues/1`);
  await expect(
    page.getByRole('heading', { name: 'Leitura da issue' }),
  ).toBeVisible();
  await expect(page.getByText('Priority fixture')).toBeVisible();
  await page.getByLabel('Prioridade', { exact: true }).selectOption('high');
  await page.getByRole('button', { name: 'Salvar prioridade' }).click();
  await expect(
    page.getByText('Prioridade alterada para Alta.', { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText('Prioridade: Sem prioridade → Alta', { exact: true }),
  ).toBeVisible();
  expect(mutationCount).toBe(1);
  expect(mutationKey).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
  );
});
