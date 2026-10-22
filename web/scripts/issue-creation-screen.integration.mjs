import assert from 'node:assert/strict';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { editWork } from './issue-text-screen.integration.mjs';

export async function createWork(context, origin, fixture, artifacts) {
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  let posts = 0;
  let fault = 'none';
  let release;
  let entered;
  let waiting;
  await page.route(
    `**/api/v1/projects/${fixture.project_id}/issues`,
    async (route) => {
      if (route.request().method() !== 'POST') {
        return route.continue();
      }
      posts += 1;
      if (fault === 'none') {
        return route.continue();
      }
      const mode = fault;
      fault = 'none';
      const response = await route.fetch();
      assert.equal(response.status(), 201);
      if (mode === 'hold') {
        entered?.();
        await waiting;
        try {
          await route.fulfill({ response });
        } catch {
          // The browser deliberately aborted on pagehide; backend already committed.
          assert.equal(mode, 'hold');
        }
      } else {
        await route.fulfill({ response, body: '{' });
      }
    },
  );
  const url = `${origin}/projects/${fixture.project_id}/issues/new`;
  await page.goto(url);
  await expect(page.locator('[data-issue-create-form]')).toBeVisible();
  await page
    .getByLabel('Título', { exact: false })
    .fill('Unsaved private draft');
  const remaining = await page.evaluate(() => {
    globalThis.dispatchEvent(new globalThis.Event('blur'));
    return document.querySelectorAll('[data-private-creation]').length;
  });
  assert.equal(remaining, 0);
  await page.evaluate(() =>
    globalThis.dispatchEvent(new globalThis.Event('focus')),
  );
  await expect(page.getByLabel('Título', { exact: false })).toHaveValue('');
  await page.getByLabel('Título', { exact: false }).fill('   ');
  await page.getByRole('button', { name: 'Criar issue', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('Confira o título');
  assert.equal(posts, 0);
  const title = 'Created <script>not executable</script>';
  await page.screenshot({
    path: join(artifacts, `${fixture.kind}-form.png`),
    fullPage: true,
  });
  await page.getByLabel('Título', { exact: false }).fill(title);
  await page
    .getByLabel('Descrição', { exact: false })
    .fill('# Plain source\n<script>throw new Error("not executable")</script>');
  await page.evaluate(() => {
    const form = document.querySelector('[data-issue-create-form]');
    if (!(form instanceof globalThis.HTMLFormElement)) {
      throw new Error('Missing form');
    }
    form.requestSubmit();
    form.requestSubmit();
  });
  await expect(
    page.getByRole('heading', { name: 'Criação confirmada' }),
  ).toBeVisible();
  assert.equal(posts, 1);
  await page.getByRole('link', { name: 'Abrir issue criada' }).click();
  await expect(page.locator('[data-issue-detail] h2')).toHaveText(title);
  await expect(page.locator('pre')).toContainText('Plain source');
  await page.goto(url);
  await expect(page.locator('[data-issue-create-form]')).toBeVisible();
  await page
    .getByLabel('Título', { exact: false })
    .fill('Uncertain private issue');
  fault = 'malformed';
  await page.getByRole('button', { name: 'Criar issue', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'Resultado não confirmado' }),
  ).toBeVisible();
  assert.equal(posts, 2);
  await expect(page.locator('[data-issue-create-form]')).toHaveCount(0);
  await page
    .getByRole('button', { name: 'Reenviar a mesma solicitação' })
    .click();
  await expect(
    page.getByRole('heading', { name: 'Criação confirmada' }),
  ).toBeVisible();
  assert.equal(posts, 3);
  await page.goto(url);
  await expect(page.locator('[data-issue-create-form]')).toBeVisible();
  await page
    .getByLabel('Título', { exact: false })
    .fill('Interrupted private issue');
  waiting = new Promise((resolve) => {
    release = resolve;
  });
  const committed = new Promise((resolve) => {
    entered = resolve;
  });
  fault = 'hold';
  await page.getByRole('button', { name: 'Criar issue', exact: true }).click();
  await committed;
  const hidden = await page.evaluate(() => {
    globalThis.dispatchEvent(
      new globalThis.PageTransitionEvent('pagehide', { persisted: true }),
    );
    return document.querySelectorAll('[data-private-creation]').length;
  });
  assert.equal(hidden, 0);
  release?.();
  await page.evaluate(() =>
    globalThis.dispatchEvent(
      new globalThis.PageTransitionEvent('pageshow', { persisted: true }),
    ),
  );
  await expect(
    page.getByRole('heading', { name: 'Resultado não confirmado' }),
  ).toBeVisible();
  assert.equal(posts, 4);
  await page
    .getByRole('button', { name: 'Reenviar a mesma solicitação' })
    .click();
  await expect(
    page.getByRole('heading', { name: 'Criação confirmada' }),
  ).toBeVisible();
  assert.equal(posts, 5);
  await page.screenshot({
    path: join(artifacts, `${fixture.kind}.png`),
    fullPage: true,
  });
  assert.equal(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= globalThis.innerWidth,
    ),
    true,
  );
  assert.equal(
    await page.evaluate(
      () => globalThis.localStorage.length + globalThis.sessionStorage.length,
    ),
    0,
  );
  await page.goto(
    `${origin}/projects/${fixture.private_project_id}/issues/new`,
  );
  await expect(
    page.getByRole('heading', { name: 'Conteúdo indisponível' }),
  ).toBeVisible();
  await expect(page.locator('[data-issue-create-form]')).toHaveCount(0);
  await page.goto(
    `${origin}/projects/${fixture.read_only_project_id}/issues/new`,
  );
  await expect(
    page.getByRole('heading', {
      name: 'Projeto somente leitura para este acesso',
    }),
  ).toBeVisible();
  await expect(page.locator('[data-issue-create-form]')).toHaveCount(0);
  assert.equal(posts, 5);
  await editWork(page, origin, fixture, artifacts);
  assert.deepEqual(errors, []);
  await page.close();
}
