import { expect, test } from '@playwright/test';

test('embedded SPA checks the real server without pretending product features exist', async ({
  page,
}) => {
  const consoleErrors: string[] = [];
  const externalRequests: string[] = [];
  page.on('pageerror', (error) => consoleErrors.push(error.message));
  page.on('request', (request) => {
    if (new URL(request.url()).origin !== 'http://127.0.0.1:4174') {
      externalRequests.push(request.url());
    }
  });
  await page.goto('/');
  await expect(page).toHaveTitle('Puxiri');
  await expect(
    page.getByRole('heading', { name: 'Trabalho em movimento.' }),
  ).toBeVisible();
  await expect(page.getByRole('status')).toContainText('Servidor pronto');
  await expect(
    page.getByText('Não use dados reais nesta versão.', { exact: false }),
  ).toBeVisible();
  await page.getByRole('button', { name: 'Verificar novamente' }).click();
  await expect(page.getByRole('status')).toContainText('Servidor pronto');
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  expect(consoleErrors).toEqual([]);
  expect(externalRequests).toEqual([]);
  await page.screenshot({
    path: test.info().outputPath('foundation.png'),
    fullPage: true,
  });
});

test('unknown API is not served as a fake application page', async ({
  request,
}) => {
  const response = await request.get('/api/v1/projects');
  expect(response.status()).toBe(404);
  expect((await response.json()).code).toBe('not_found');
});

test('a real network interruption displays an actionable error and can recover', async ({
  page,
  context,
}) => {
  await page.goto('/');
  await expect(page.getByRole('status')).toContainText('Servidor pronto');
  await context.setOffline(true);
  await page.getByRole('button', { name: 'Verificar novamente' }).click();
  await expect(page.getByRole('status')).toContainText(
    'Não foi possível conectar',
  );
  await expect(
    page.getByText('Confira se o processo está em execução e tente novamente.'),
  ).toBeVisible();
  await context.setOffline(false);
  await page.getByRole('button', { name: 'Verificar novamente' }).click();
  await expect(page.getByRole('status')).toContainText('Servidor pronto');
});
