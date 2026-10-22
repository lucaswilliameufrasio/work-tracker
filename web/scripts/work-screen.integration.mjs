import assert from 'node:assert/strict';
import { join } from 'node:path';
import { expect } from '@playwright/test';

// Called only by the synthetic loopback HTTPS/SQLite fixture.
export async function readWork(context, origin, fixture, artifacts) {
  const page = await context.newPage();
  const errors = [];
  const metadataRequests = [];
  page.on('request', (request) => {
    const path = new URL(request.url()).pathname;
    if (path.includes('/statuses') || path.includes('/issue-types')) {
      metadataRequests.push(path);
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
  await page.locator(`[data-project-id="${fixture.project_id}"] a`).click();
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
  await expect(page.locator('.journal li')).toHaveCount(1);
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
  assert.ok(
    metadataRequests.every((path) =>
      /\/(statuses|issue-types)\/[0-9a-f-]{36}$/.test(path),
    ),
  );
  await page.close();
}
