import { expect, test } from '@playwright/test';

test('session screen explains disabled API without offering mutations or fake login', async ({
  page,
}) => {
  await page.goto('/');
  await page.getByRole('link', { name: 'Minhas sessões' }).click();
  await expect(
    page.getByRole('heading', { name: 'Minhas sessões', exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole('heading', { name: 'Gestão de sessões não habilitada' }),
  ).toBeVisible();
  await expect(
    page.getByRole('button', { name: 'Encerrar todas as sessões' }),
  ).toHaveCount(0);
  await expect(page.getByRole('link', { name: /login/i })).toHaveCount(0);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= globalThis.innerWidth,
    ),
  ).toBe(true);
  await page.goBack();
  await expect(
    page.getByRole('heading', { name: 'Trabalho em movimento.' }),
  ).toBeVisible();
});
