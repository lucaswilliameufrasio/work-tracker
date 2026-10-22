import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';
import { expect } from '@playwright/test';

export async function editWork(page, origin, fixture, artifacts) {
  const endpoint = `/api/v1/projects/${fixture.project_id}/issues/1/text`;
  let puts = 0;
  let lose = false;
  const commands = [];
  await page.route(`**${endpoint}`, async (route) => {
    puts += 1;
    commands.push({
      key: route.request().headers()['idempotency-key'],
      body: route.request().postData(),
    });
    if (!lose) {
      return route.continue();
    }
    lose = false;
    const response = await route.fetch();
    assert.equal(response.status(), 200);
    await route.fulfill({ response, body: '{' });
  });
  await page.goto(`${origin}/projects/${fixture.project_id}/issues/1/edit`);
  await expect(page.locator('[data-issue-text-form]')).toBeVisible();
  const draft = 'Preserved <script>not executable</script>';
  await page.getByLabel('Título', { exact: false }).fill(draft);
  assert.equal(
    await page.evaluate(() => {
      globalThis.dispatchEvent(new globalThis.Event('blur'));
      return document.querySelectorAll('[data-private-editor]').length;
    }),
    0,
  );
  await page.evaluate(() =>
    globalThis.dispatchEvent(new globalThis.Event('focus')),
  );
  await expect(page.getByLabel('Título', { exact: false })).toHaveValue(draft);
  const sessionResponse = await page.request.get(
    `${origin}/api/v1/auth/session`,
  );
  const session = await sessionResponse.json();
  const concurrent = await page.request.put(`${origin}${endpoint}`, {
    headers: {
      Origin: origin,
      'x-csrf-token': session.csrf_token,
      'Idempotency-Key': randomUUID(),
    },
    data: { title: 'Concurrent real change', expected_version: '1' },
  });
  assert.equal(concurrent.status(), 200);
  await page.getByRole('button', { name: 'Salvar texto', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'A issue mudou', exact: true }),
  ).toBeVisible();
  await expect(page.locator('[data-edit-draft]')).toContainText(draft);
  assert.equal(puts, 1);
  await page
    .getByRole('button', { name: 'Carregar versão atual para revisão' })
    .click();
  await expect(page.locator('[data-edit-current]')).toContainText(
    'Concurrent real change',
  );
  await page.screenshot({
    path: join(artifacts, `${fixture.kind}-edit-review.png`),
    fullPage: true,
  });
  await page
    .getByRole('button', { name: 'Usar meu rascunho na versão 2' })
    .click();
  assert.equal(puts, 1);
  await expect(page.getByLabel('Título', { exact: false })).toHaveValue(draft);
  await page.getByRole('button', { name: 'Salvar texto', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'Edição confirmada', exact: true }),
  ).toBeVisible();
  assert.equal(puts, 2);
  assert.notEqual(commands[0].key, commands[1].key);
  await page
    .getByRole('link', { name: 'Editar novamente', exact: true })
    .click();
  await expect(page.getByLabel('Título', { exact: false })).toHaveValue(draft);
  await page.getByLabel('Título', { exact: false }).fill('Recovered edit');
  lose = true;
  await page.getByRole('button', { name: 'Salvar texto', exact: true }).click();
  await expect(
    page.getByRole('heading', {
      name: 'Resultado não confirmado',
      exact: true,
    }),
  ).toBeVisible();
  await page.evaluate(() =>
    globalThis.dispatchEvent(
      new globalThis.PageTransitionEvent('pagehide', { persisted: true }),
    ),
  );
  await expect(page.locator('[data-private-editor]')).toHaveCount(0);
  await page.evaluate(() =>
    globalThis.dispatchEvent(
      new globalThis.PageTransitionEvent('pageshow', { persisted: true }),
    ),
  );
  await expect(
    page.getByRole('heading', {
      name: 'Resultado não confirmado',
      exact: true,
    }),
  ).toBeVisible();
  assert.equal(puts, 3);
  await page
    .getByRole('button', { name: 'Reenviar a mesma edição', exact: true })
    .click();
  await expect(
    page.getByRole('heading', { name: 'Edição confirmada', exact: true }),
  ).toBeVisible();
  assert.equal(puts, 4);
  assert.deepEqual(commands[2], commands[3]);
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= globalThis.innerWidth,
    ),
    true,
  );
  for (const project of [
    fixture.private_project_id,
    fixture.read_only_project_id,
  ]) {
    await page.goto(`${origin}/projects/${project}/issues/1/edit`);
    await expect(
      page.getByRole('heading', {
        name: 'Edição indisponível para este acesso',
        exact: true,
      }),
    ).toBeVisible();
    await expect(page.locator('[data-issue-text-form]')).toHaveCount(0);
  }
  assert.equal(puts, 4);
}
