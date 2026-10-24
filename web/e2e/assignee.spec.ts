import { expect, test } from '@playwright/test';

const projectId = '019a0000-0000-7000-8000-000000000001';
const issueId = '019a0000-0000-7000-8000-000000000002';
const statusId = '019a0000-0000-7000-8000-000000000003';
const issueTypeId = '019a0000-0000-7000-8000-000000000004';
const actorId = '019a0000-0000-7000-8000-000000000005';
const memberId = '019a0000-0000-7000-8000-000000000006';
const viewerId = '019a0000-0000-7000-8000-000000000007';
const creationEventId = '019a0000-0000-7000-8000-000000000008';
const assignEventId = '019a0000-0000-7000-8000-000000000009';
const clearEventId = '019a0000-0000-7000-8000-000000000010';
const now = '2026-10-24T10:00:00.000000000Z';
const changedAt = '2026-10-24T10:01:00.000000000Z';

test('issue detail assigns an eligible member and clears assignment explicitly', async ({
  page,
}) => {
  let assigneeUserId: string | null = null;
  let version = '1';
  const mutationBodies: Array<Record<string, unknown>> = [];
  const mutationKeys: string[] = [];
  const issue = () => ({
    id: issueId,
    project_id: projectId,
    issue_number: '1',
    issue_key: 'DEV-1',
    issue_type_id: issueTypeId,
    status_id: statusId,
    title: 'Assignment fixture',
    description: 'Private text stays outside history.',
    ...(assigneeUserId === null ? {} : { assignee_user_id: assigneeUserId }),
    creator_user_id: actorId,
    priority: 'none',
    is_overdue: false,
    version,
    created_at: now,
    updated_at: version === '1' ? now : changedAt,
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
    if (version === '2' || version === '3') {
      result.push({
        id: assignEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.field_updated',
        change: {
          field: 'assignee_user_id',
          before: null,
          after: memberId,
        },
        occurred_at: changedAt,
      });
    }
    if (version === '3') {
      result.push({
        id: clearEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.field_updated',
        change: {
          field: 'assignee_user_id',
          before: memberId,
          after: null,
        },
        occurred_at: '2026-10-24T10:02:00.000000000Z',
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
      path === `/api/v1/projects/${projectId}/members` &&
      request.method() === 'GET'
    ) {
      await route.fulfill({
        json: {
          data: [
            {
              user_id: memberId,
              display_name: 'Eligible member',
              role: 'member',
              can_be_assigned: true,
            },
            {
              user_id: viewerId,
              display_name: 'Read-only viewer',
              role: 'viewer',
              can_be_assigned: false,
            },
          ],
          has_more: false,
        },
      });
      return;
    }
    if (
      path === `/api/v1/projects/${projectId}/members/${memberId}` &&
      request.method() === 'GET'
    ) {
      await route.fulfill({
        json: {
          user_id: memberId,
          display_name: 'Eligible member',
          role: 'member',
          can_be_assigned: true,
        },
      });
      return;
    }
    if (
      path === `/api/v1/projects/${projectId}/issues/1/assignee` &&
      request.method() === 'PUT'
    ) {
      const body = request.postDataJSON() as Record<string, unknown>;
      mutationBodies.push(body);
      mutationKeys.push(request.headers()['idempotency-key'] ?? '');
      expect(request.headers()['x-csrf-token']).toBe('A'.repeat(43));
      expect(request.headers().origin).toBe(url.origin);
      expect(Object.keys(body).sort()).toEqual([
        'assignee_user_id',
        'expected_version',
      ]);
      expect(body.expected_version).toBe(version);
      assigneeUserId = body.assignee_user_id as string | null;
      version = String(BigInt(version) + 1n);
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
          assignee_user_id: assigneeUserId,
          result_version: version,
          changed: true,
        },
      });
      return;
    }
    await route.fulfill({ status: 404, json: {} });
  });

  await page.goto(`/projects/${projectId}/issues/1`);
  await expect(page.locator('[data-current-assignee="none"]')).toBeVisible();
  const picker = page.getByLabel('Atribuir a');
  await expect(picker.locator(`option[value="${memberId}"]`)).toHaveText(
    'Eligible member · membro',
  );
  await expect(picker.locator(`option[value="${viewerId}"]`)).toHaveCount(0);

  await picker.selectOption(memberId);
  await page.getByRole('button', { name: 'Salvar responsável' }).click();
  await expect(
    page.getByText('Responsável atualizado.', { exact: true }),
  ).toBeVisible();
  await expect(
    page.locator(`[data-current-assignee="${memberId}"]`),
  ).toContainText('Eligible member');
  await expect(
    page.getByText(`Responsável: Sem responsável → ${memberId}`),
  ).toBeVisible();

  await picker.selectOption('');
  await page.getByRole('button', { name: 'Salvar responsável' }).click();
  await expect(
    page.getByText('Responsável removido.', { exact: true }),
  ).toBeVisible();
  await expect(page.locator('[data-current-assignee="none"]')).toBeVisible();
  await expect(
    page.getByText(`Responsável: ${memberId} → Sem responsável`),
  ).toBeVisible();
  expect(mutationBodies).toEqual([
    { expected_version: '1', assignee_user_id: memberId },
    { expected_version: '2', assignee_user_id: null },
  ]);
  expect(mutationKeys).toHaveLength(2);
  expect(mutationKeys[0]).not.toBe(mutationKeys[1]);
  expect(mutationKeys.every((key) => /^[0-9a-f-]{36}$/.test(key))).toBe(true);
});
