import assert from 'node:assert/strict';
import { join } from 'node:path';
import { expect } from '@playwright/test';

// The issues/search selector reads one member page of 100. The detail editor
// reads one roster page of 50. This journey must not continue either page.
function boundedMemberCatalog(url) {
  const pageSize = url.searchParams.get('page_size');
  return (
    url.pathname.endsWith('/members') &&
    url.searchParams.get('cursor') === null &&
    [...url.searchParams.keys()].length === 1 &&
    (pageSize === '100' || pageSize === '50')
  );
}

function boundedWorkCatalog(address) {
  const url = new URL(address);
  return (
    /\/(statuses|issue-types)\/[0-9a-f-]{36}$/.test(url.pathname) ||
    (url.pathname.endsWith('/statuses') &&
      url.searchParams.get('page_size') === '4' &&
      url.searchParams.get('include_archived') === 'false') ||
    (url.pathname.endsWith('/issue-types') &&
      url.searchParams.get('page_size') === '100' &&
      url.searchParams.get('include_archived') === 'false' &&
      url.searchParams.get('cursor') === null) ||
    (url.pathname.endsWith('/statuses') &&
      url.searchParams.get('page_size') === '100' &&
      url.searchParams.get('include_archived') === 'false' &&
      url.searchParams.get('cursor') === null) ||
    boundedMemberCatalog(url) ||
    /\/members\/[0-9a-f-]{36}$/.test(url.pathname)
  );
}

// Called only by the synthetic loopback HTTPS/SQLite fixture.
export async function readWork(context, origin, fixture, artifacts) {
  const page = await context.newPage();
  const errors = [];
  const metadataRequests = [];
  page.on('request', (request) => {
    const path = new URL(request.url()).pathname;
    if (
      path.includes('/statuses') ||
      path.includes('/issue-types') ||
      path.includes('/members')
    ) {
      metadataRequests.push(request.url());
    }
  });
  page.on('pageerror', (error) => errors.push(error.message));
  const response = await page.goto(`${origin}/projects`);
  assert.equal(response.headers()['cache-control'], 'no-store');
  await expect(page.locator('[data-project-id]')).toHaveCount(1);
  await expect(
    page.getByText('Private outsider project', { exact: true }),
  ).toHaveCount(0);
  await page
    .getByRole('link', { name: 'Incluir projetos arquivados', exact: true })
    .click();
  await expect(page.locator('[data-project-id]')).toHaveCount(2);
  await page
    .locator(
      `[data-project-id="${fixture.project_id}"] a[href="/projects/${fixture.project_id}/issues"]`,
    )
    .click();
  await expect(page.locator('[data-issue-id]')).toHaveCount(20);
  await page.getByRole('link', { name: 'Ver board', exact: true }).click();
  await expect(page.locator('[data-board-column]')).toHaveCount(3);
  await expect(page.locator('[data-board-card]')).toHaveCount(20);
  await expect(
    page.getByText('21 issues fora da lixeira', { exact: true }),
  ).toHaveCount(1);
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= globalThis.innerWidth,
    ),
    true,
  );
  await page.screenshot({
    path: join(artifacts, `${fixture.kind}-board.png`),
    fullPage: true,
  });
  const sourceColumn = page
    .locator('[data-board-column]')
    .filter({ has: page.getByRole('heading', { name: 'To do', exact: true }) });
  const targetColumn = page.locator('[data-board-column]').filter({
    has: page.getByRole('heading', { name: 'In progress', exact: true }),
  });
  const targetStatusId = await targetColumn.getAttribute('data-board-column');
  const sourceStatusId = await sourceColumn.getAttribute('data-board-column');
  const movingCard = page
    .getByRole('link', {
      name: 'READ-1 Synthetic work issue 1',
      exact: true,
    })
    .locator('xpath=ancestor::li[@data-board-card]');
  const movingIssueId = await movingCard.getAttribute('data-board-card');
  assert.ok(targetStatusId);
  assert.ok(sourceStatusId);
  assert.ok(movingIssueId);
  const issueResponse = await page.request.get(
    `${origin}/api/v1/projects/${fixture.project_id}/issues/1`,
  );
  assert.equal(issueResponse.status(), 200);
  const issueState = await issueResponse.json();
  const moveEndpoint = `${origin}/api/v1/projects/${fixture.project_id}/issues/1/move`;
  const moveCommands = [];
  const replayHeaders = [];
  await page.route(moveEndpoint, async (route) => {
    moveCommands.push({
      key: route.request().headers()['idempotency-key'],
      body: route.request().postData(),
    });
    if (moveCommands.length === 1) {
      const committed = await route.fetch();
      assert.equal(committed.status(), 200);
      assert.equal(committed.headers()['idempotency-replayed'], 'false');
      await route.fulfill({ response: committed, body: '{' });
      return;
    }
    await route.continue();
  });
  page.on('response', (moveResponse) => {
    if (moveResponse.url() === moveEndpoint) {
      replayHeaders.push(moveResponse.headers()['idempotency-replayed']);
    }
  });
  await movingCard.getByText('Movimentar', { exact: true }).click();
  await movingCard.getByLabel('Coluna de destino').selectOption(targetStatusId);
  await movingCard.getByRole('button', { name: 'Mover para o topo' }).click();
  await expect(page.getByRole('alert')).toContainText(
    'Não foi possível confirmar o movimento',
  );
  await page
    .getByRole('button', { name: 'Repetir o mesmo comando com a mesma chave' })
    .click();
  await expect(
    page
      .getByRole('status')
      .filter({ hasText: 'Movimento de READ-1 confirmado.' }),
  ).toBeVisible();
  assert.equal(moveCommands.length, 2);
  assert.deepEqual(moveCommands[0], moveCommands[1]);
  assert.deepEqual(replayHeaders, ['false', 'true']);
  const submittedMove = JSON.parse(moveCommands[0].body);
  assert.deepEqual(submittedMove, {
    expected_version: issueState.version,
    target_status_id: targetStatusId,
  });
  await expect(sourceColumn.locator('[data-board-card]')).toHaveCount(20);
  await expect(targetColumn.locator('[data-board-card]')).toHaveCount(1);
  await expect(
    targetColumn.locator(`[data-board-card="${movingIssueId}"]`),
  ).toHaveCount(1);
  await expect(
    sourceColumn.locator(`[data-board-card="${movingIssueId}"]`),
  ).toHaveCount(0);
  const movedCard = targetColumn.locator(
    `[data-board-card="${movingIssueId}"]`,
  );
  const restoredIssueResponse = await page.request.get(
    `${origin}/api/v1/projects/${fixture.project_id}/issues/1`,
  );
  assert.equal(restoredIssueResponse.status(), 200);
  const movedIssueState = await restoredIssueResponse.json();
  const restoreNeighborId = await sourceColumn
    .locator('[data-board-card]')
    .first()
    .getAttribute('data-board-card');
  assert.ok(restoreNeighborId);
  await movedCard.getByText('Movimentar', { exact: true }).click();
  await movedCard.getByLabel('Coluna de destino').selectOption(sourceStatusId);
  const commandCountBeforeRestore = moveCommands.length;
  await movedCard.getByRole('button', { name: 'Mover para o topo' }).click();
  await expect(sourceColumn.locator('[data-board-card]')).toHaveCount(20);
  await expect(sourceColumn.getByText('21 issues fora da lixeira')).toHaveCount(
    1,
  );
  await expect(targetColumn.locator('[data-board-card]')).toHaveCount(0);
  assert.equal(moveCommands.length, commandCountBeforeRestore + 1);
  assert.deepEqual(replayHeaders, ['false', 'true', 'false']);
  assert.notEqual(moveCommands[0].key, moveCommands[2].key);
  assert.deepEqual(JSON.parse(moveCommands[commandCountBeforeRestore].body), {
    expected_version: movedIssueState.version,
    target_status_id: sourceStatusId,
    next_issue_id: restoreNeighborId,
  });
  await page
    .getByRole('link', { name: 'Continuar nesta coluna', exact: true })
    .click();
  await expect(page.locator('[data-board-column]')).toHaveCount(1);
  await expect(page.locator('[data-board-card]')).toHaveCount(1);
  await expect(
    page.getByRole('link', {
      name: 'READ-22 Synthetic work issue 22',
      exact: true,
    }),
  ).toBeVisible();
  await page.goto(`${origin}/projects/${fixture.project_id}/issues`);
  await expect(page.locator('[data-issue-id]')).toHaveCount(20);
  await expect(
    page.getByText('Synthetic work issue 2', { exact: true }),
  ).toHaveCount(0);
  await page
    .getByRole('link', { name: 'Próxima página de issues', exact: true })
    .click();
  await expect(page.locator('[data-issue-id]')).toHaveCount(2);
  await page
    .getByRole('link', { name: 'READ-23 Synthetic work issue 23', exact: true })
    .focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('[data-issue-detail]')).toHaveCount(1);
  await expect(
    page.getByRole('heading', { name: 'Synthetic work issue 23', exact: true }),
  ).toBeVisible();
  await expect(
    page
      .locator('dd')
      .filter({ hasText: /^Referenced status beyond page100$/ }),
  ).toHaveCount(1);
  await expect(
    page.locator('dd').filter({ hasText: /^Referenced type beyond page100$/ }),
  ).toHaveCount(1);
  await expect(page.locator('pre')).toContainText('<script>throw new Error');
  await expect(page.locator('pre')).toContainText(
    'https://forbidden.example.test/private.png',
  );
  await expect(page.locator('[data-private-work] img')).toHaveCount(0);
  await expect(page.locator('.journal li')).toHaveCount(4);
  for (const label of [
    'Issue criada',
    'Título alterado',
    'Descrição alterada',
    'Título e descrição alterados',
  ]) {
    await expect(
      page.locator('.journal').getByText(label, { exact: true }),
    ).toBeVisible();
  }
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= globalThis.innerWidth,
    ),
    true,
  );
  await page.screenshot({
    path: join(artifacts, `${fixture.kind}-detail.png`),
    fullPage: true,
  });
  await page
    .getByRole('link', { name: 'Voltar à lista de issues', exact: true })
    .click();
  assert.ok(page.url().endsWith('/issues?cursor=21'));
  await expect(page.locator('[data-issue-id]')).toHaveCount(2);
  await page.goto(`${origin}/projects/${fixture.project_id}/issues/2`);
  await expect(page.getByRole('status')).toContainText('Na lixeira');
  await page.goto(`${origin}/projects/${fixture.private_project_id}/issues`);
  await expect(
    page.getByRole('heading', { name: 'Conteúdo indisponível', exact: true }),
  ).toBeVisible();
  await expect(page.locator('[data-private-work]')).toHaveCount(0);
  await page.goto(`${origin}/projects/${fixture.project_id}/issues/999`);
  await expect(
    page.getByRole('heading', { name: 'Conteúdo indisponível', exact: true }),
  ).toBeVisible();
  await page.goto(`${origin}/projects/${fixture.project_id}/issues/23`);
  await expect(page.locator('[data-issue-detail]')).toHaveCount(1);
  await page.getByRole('link', { name: 'Ver board', exact: true }).click();
  await expect(page.locator('[data-board-column]')).toHaveCount(3);
  const remaining = await page.evaluate(() => {
    globalThis.dispatchEvent(
      new globalThis.PageTransitionEvent('pagehide', { persisted: true }),
    );
    return document.querySelectorAll('[data-private-work]').length;
  });
  assert.equal(remaining, 0);
  const current = await context.request.get(`${origin}/api/v1/auth/session`);
  assert.equal(current.status(), 200);
  const session = await current.json();
  const revoked = await context.request.delete(
    `${origin}/api/v1/auth/session`,
    { headers: { origin, 'x-csrf-token': session.csrf_token } },
  );
  assert.equal(revoked.status(), 204);
  await page.evaluate(() =>
    globalThis.dispatchEvent(
      new globalThis.PageTransitionEvent('pageshow', { persisted: true }),
    ),
  );
  await expect(
    page.getByRole('heading', {
      name: 'Sessão encerrada ou expirada',
      exact: true,
    }),
  ).toBeVisible();
  await expect(page.locator('[data-private-work]')).toHaveCount(0);
  assert.equal(
    await page.evaluate(
      () => globalThis.localStorage.length + globalThis.sessionStorage.length,
    ),
    0,
  );
  assert.deepEqual(errors, []);
  assert.ok(metadataRequests.length > 0);
  const unexpectedMetadata = [];
  for (const address of metadataRequests) {
    if (!boundedWorkCatalog(address)) {
      unexpectedMetadata.push(address);
    }
  }
  assert.deepEqual(unexpectedMetadata, []);
  assert.ok(
    metadataRequests.some((address) => {
      const url = new URL(address);
      return (
        url.pathname.endsWith('/issue-types') &&
        url.searchParams.get('page_size') === '100' &&
        url.searchParams.get('include_archived') === 'false' &&
        url.searchParams.get('cursor') === null
      );
    }),
  );
  assert.ok(
    metadataRequests.some((address) => {
      const url = new URL(address);
      return (
        url.pathname.endsWith('/statuses') &&
        url.searchParams.get('page_size') === '100' &&
        url.searchParams.get('include_archived') === 'false' &&
        url.searchParams.get('cursor') === null
      );
    }),
  );
  assert.ok(
    metadataRequests.some((address) => {
      const url = new URL(address);
      return (
        url.pathname.endsWith('/members') &&
        url.searchParams.get('page_size') === '100' &&
        url.searchParams.get('cursor') === null
      );
    }),
  );
  await page.close();
}
