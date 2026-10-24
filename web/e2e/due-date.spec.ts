import { expect, test } from '@playwright/test';

const projectId = '019a0000-0000-7000-8000-000000000001';
const issueId = '019a0000-0000-7000-8000-000000000002';
const statusId = '019a0000-0000-7000-8000-000000000003';
const issueTypeId = '019a0000-0000-7000-8000-000000000004';
const actorId = '019a0000-0000-7000-8000-000000000005';
const creationEventId = '019a0000-0000-7000-8000-000000000006';
const conflictEventId = '019a0000-0000-7000-8000-000000000007';
const setEventId = '019a0000-0000-7000-8000-000000000008';
const clearEventId = '019a0000-0000-7000-8000-000000000009';
const errorRequestId = '019a0000-0000-7000-8000-000000000010';
const now = '2026-10-21T10:00:00.000000000Z';
const changedAt = '2026-10-21T10:01:00.000000000Z';

test('issue detail sets and clears a civil due date with explicit commands', async ({
  page,
}) => {
  let dueDate: string | null = null;
  let overdue = false;
  let version = '1';
  let mutationCount = 0;
  const mutationKeys: string[] = [];
  let uncertainKey: string | null = null;
  let uncertainBody: {
    expected_version: string;
    due_date: string | null;
  } | null = null;
  let uncertainReceipt: Record<string, unknown> | null = null;
  const issue = () => ({
    id: issueId,
    project_id: projectId,
    issue_number: '1',
    issue_key: 'DEV-1',
    issue_type_id: issueTypeId,
    status_id: statusId,
    title: 'Due-date fixture',
    description: 'Private text stays outside history.',
    creator_user_id: actorId,
    priority: 'none',
    ...(dueDate === null ? {} : { due_date: dueDate }),
    is_overdue: overdue,
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
    if (version !== '1') {
      result.push({
        id: conflictEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.field_updated',
        change: { field: 'due_date', before: null, after: '2025-12-31' },
        occurred_at: changedAt,
      });
    }
    if (version === '3' || version === '4') {
      result.push({
        id: setEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.field_updated',
        change: {
          field: 'due_date',
          before: '2025-12-31',
          after: '2026-01-02',
        },
        occurred_at: '2026-10-21T10:02:00.000000000Z',
      });
    }
    if (version === '4') {
      result.push({
        id: clearEventId,
        project_id: projectId,
        issue_id: issueId,
        actor_user_id: actorId,
        event_kind: 'issue.field_updated',
        change: { field: 'due_date', before: '2026-01-02', after: null },
        occurred_at: '2026-10-21T10:03:00.000000000Z',
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
      path === `/api/v1/projects/${projectId}/issues/1/due-date` &&
      request.method() === 'PUT'
    ) {
      mutationCount += 1;
      const idempotencyKey = request.headers()['idempotency-key'] ?? '';
      mutationKeys.push(idempotencyKey);
      expect(request.headers()['x-csrf-token']).toBe('A'.repeat(43));
      expect(request.headers().origin).toBe(url.origin);
      const body = request.postDataJSON() as {
        expected_version: string;
        due_date: string | null;
      };
      expect(Object.keys(body).sort()).toEqual([
        'due_date',
        'expected_version',
      ]);
      if (uncertainKey !== null && idempotencyKey === uncertainKey) {
        expect(body).toEqual(uncertainBody);
        await route.fulfill({
          status: 200,
          headers: {
            'cache-control': 'no-store',
            location: `/api/v1/projects/${projectId}/issues/1`,
            'idempotency-replayed': 'true',
          },
          json: uncertainReceipt,
        });
        return;
      }
      expect(body.expected_version).toBe(version);
      if (version === '1' && body.due_date === '2026-01-02') {
        dueDate = '2025-12-31';
        overdue = true;
        version = '2';
        await route.fulfill({
          status: 409,
          json: {
            code: 'issue_version_conflict',
            message: 'The issue changed',
            request_id: errorRequestId,
            field_errors: {},
            retryable: false,
          },
        });
        return;
      }
      if (body.due_date === '2026-01-02') {
        dueDate = body.due_date;
        overdue = true;
      } else {
        expect(body.due_date).toBeNull();
        dueDate = null;
        overdue = false;
      }
      version = String(BigInt(version) + 1n);
      const receipt = {
        id: issueId,
        project_id: projectId,
        issue_number: '1',
        issue_key: 'DEV-1',
        due_date: dueDate,
        result_version: version,
        changed: true,
      };
      if (body.due_date === '2026-01-02') {
        uncertainKey = idempotencyKey;
        uncertainBody = body;
        uncertainReceipt = receipt;
        await route.fulfill({
          status: 503,
          json: {
            code: 'storage_unavailable',
            message: 'Storage is unavailable',
            request_id: errorRequestId,
            field_errors: {},
            retryable: false,
          },
        });
        return;
      }
      await route.fulfill({
        status: 200,
        headers: {
          'cache-control': 'no-store',
          location: `/api/v1/projects/${projectId}/issues/1`,
          'idempotency-replayed': 'false',
        },
        json: receipt,
      });
      return;
    }
    await route.fulfill({ status: 404, json: { code: 'not_found' } });
  });

  await page.goto(`/projects/${projectId}/issues/1`);
  await expect(
    page.getByRole('heading', { name: 'Leitura da issue' }),
  ).toBeVisible();
  const dateInput = page.getByLabel('Prazo · data civil');
  await expect(dateInput).toHaveValue('');
  await dateInput.fill('2026-01-02');
  await page.getByRole('button', { name: 'Salvar prazo' }).click();
  await expect(page.getByText(/A issue mudou\./)).toBeVisible();
  await expect(dateInput).toHaveValue('2026-01-02');
  await expect(page.getByText('Prazo: Sem prazo → 2025-12-31')).toBeVisible();
  expect(mutationCount).toBe(1);
  await page.getByRole('button', { name: 'Salvar prazo' }).click();
  await expect(
    page.getByRole('button', {
      name: 'Repetir o mesmo comando com a mesma chave',
    }),
  ).toBeVisible();
  expect(mutationCount).toBe(2);
  await page
    .getByRole('button', {
      name: 'Repetir o mesmo comando com a mesma chave',
    })
    .click();
  await expect(page.getByText('Prazo definido para 2026-01-02.')).toBeVisible();
  await expect(page.getByText('Prazo: 2025-12-31 → 2026-01-02')).toBeVisible();
  await expect(page.getByText('2026-01-02 · Atrasada')).toBeVisible();

  await page.getByRole('button', { name: 'Limpar prazo' }).click();
  await expect(page.getByText('Prazo removido.')).toBeVisible();
  await expect(page.getByText('Prazo: 2026-01-02 → Sem prazo')).toBeVisible();
  expect(mutationCount).toBe(4);
  expect(mutationKeys[0]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
  );
  expect(mutationKeys[1]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
  );
  expect(mutationKeys[2]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
  );
  expect(mutationKeys[3]).toMatch(
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
  );
  expect(mutationKeys[0]).not.toBe(mutationKeys[1]);
  expect(mutationKeys[1]).toBe(mutationKeys[2]);
  expect(mutationKeys[2]).not.toBe(mutationKeys[3]);
});
