// Real browser + ephemeral TLS proxy for a Rust-owned SQLite fixture only.
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { readFileSync } from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { URL } from 'node:url';
import { chromium, expect } from '@playwright/test';

const backend = new URL(process.env.PUXIRI_TEST_BACKEND ?? '');
assert.equal(backend.hostname, '127.0.0.1');
const tls = process.env.PUXIRI_TEST_TLS_DIR;
assert.ok(tls);
let dropNextDelete;
let dropped = 0;
let mutations = 0;
const proxy = https.createServer(
  {
    key: readFileSync(join(tls, 'key.pem')),
    cert: readFileSync(join(tls, 'certificate.pem')),
  },
  (request, response) => {
    const upstream = http.request(
      {
        hostname: '127.0.0.1',
        port: backend.port,
        path: request.url,
        method: request.method,
        headers: request.headers,
      },
      (reply) => {
        if (
          request.method === 'DELETE' &&
          dropNextDelete &&
          reply.statusCode === 204
        ) {
          const fault = dropNextDelete;
          dropNextDelete = undefined;
          dropped += 1;
          reply.on('end', () => {
            if (fault === 'partial-body') {
              // The API has committed. Send incomplete non-success framing so
              // the browser sees headers before connection loss, no cookie.
              response.writeHead(200, {
                'content-type': 'application/json',
                'content-length': '100',
              });
              response.flushHeaders();
              response.write('{', () => response.destroy());
            } else {
              response.destroy();
            }
          });
          reply.resume();
          return;
        }
        response.writeHead(reply.statusCode ?? 502, reply.headers);
        reply.pipe(response);
      },
    );
    upstream.on('error', () => {
      response.destroy();
    });
    request.on('error', () => upstream.destroy());
    if (
      request.method === 'DELETE' &&
      request.url?.startsWith('/api/v1/auth/')
    ) {
      mutations += 1;
    }
    request.pipe(upstream);
  },
);
await new Promise((resolve) => proxy.listen(0, '127.0.0.1', resolve));
const address = proxy.address();
assert.ok(address && typeof address === 'object');
const origin = `https://127.0.0.1:${address.port}`;
process.stdout.write(`${JSON.stringify({ origin })}\n`);
const input = createInterface({ input: process.stdin });
const [line] = await once(input, 'line');
input.close();
const configuration = JSON.parse(line);
const browser = await chromium.launch({ headless: true });
try {
  for (const fixture of configuration.fixtures) {
    const context = await browser.newContext({
      ignoreHTTPSErrors: true,
      serviceWorkers: 'block',
      viewport:
        fixture.kind === 'global'
          ? { width: 375, height: 812 }
          : { width: 1280, height: 900 },
      colorScheme: fixture.kind === 'global' ? 'dark' : 'light',
    });
    const errors = [];
    const external = [];
    await context.route('**/*', (route) => {
      if (new URL(route.request().url()).origin !== origin) {
        external.push('external request');
        return route.abort();
      }
      return route.continue();
    });
    await context.addCookies([
      {
        name: '__Host-puxiri_session',
        value: fixture.token,
        url: origin,
        secure: true,
        httpOnly: true,
        sameSite: 'Lax',
      },
    ]);
    const cookies = await context.cookies(origin);
    const sessionCookie = cookies.find(
      (cookie) => cookie.name === '__Host-puxiri_session',
    );
    assert.ok(sessionCookie);
    assert.equal(sessionCookie.secure, true);
    assert.equal(sessionCookie.httpOnly, true);
    assert.equal(sessionCookie.sameSite, 'Lax');
    assert.equal(sessionCookie.path, '/');
    const page = await context.newPage();
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto(`${origin}/settings/sessions`);
    await expect(page.getByRole('status')).toContainText(
      'Nenhuma sessão ativa nesta página.',
    );
    await page.getByRole('button', { name: 'Próxima página' }).click();
    await expect(page.locator('[data-session-id]')).toHaveCount(2);
    await expect(
      page.getByRole('heading', { name: 'Sessão atual', exact: true }),
    ).toBeVisible();
    assert.equal(
      await page.evaluate(() =>
        document.cookie.includes('__Host-puxiri_session'),
      ),
      false,
    );
    assert.equal(
      await page.evaluate(
        () => globalThis.localStorage.length + globalThis.sessionStorage.length,
      ),
      0,
    );
    // Actual same-origin browser DELETE without CSRF must fail before mutation.
    assert.equal(
      await page.evaluate(
        async () =>
          (
            await fetch('/api/v1/auth/session', {
              method: 'DELETE',
              credentials: 'same-origin',
            })
          ).status,
      ),
      403,
    );
    const before = mutations;
    const target = page
      .locator(`[data-session-id="${fixture.other_id}"]`)
      .getByRole('button');
    await target.click();
    await expect(
      page.getByRole('heading', { name: 'Encerrar esta sessão?' }),
    ).toBeFocused();
    await page
      .getByRole('button', { name: 'Cancelar', exact: true })
      .press('Enter');
    await expect(target).toBeFocused();
    assert.equal(mutations, before);
    await context.setOffline(true);
    await page.getByRole('button', { name: 'Voltar ao início' }).click();
    await expect(
      page.getByRole('heading', {
        name: 'Não foi possível carregar as sessões',
      }),
    ).toBeVisible();
    await expect(page.locator('[data-session-id]')).toHaveCount(0);
    await context.setOffline(false);
    await page
      .getByRole('button', { name: 'Consultar novamente', exact: true })
      .click();
    await expect(page.getByRole('status')).toContainText(
      'Nenhuma sessão ativa nesta página.',
    );
    await page.getByRole('button', { name: 'Próxima página' }).click();
    await expect(page.locator('[data-session-id]')).toHaveCount(2);
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= globalThis.innerWidth,
      ),
      true,
    );
    await page.screenshot({
      path: join(configuration.artifacts, `sessions-${fixture.kind}.png`),
      fullPage: true,
    });
    await target.click();
    await page
      .getByRole('button', { name: 'Confirmar encerramento', exact: true })
      .click();
    await expect(
      page.getByRole('status').filter({ hasText: 'A sessão foi encerrada.' }),
    ).toBeVisible();
    assert.equal(mutations, before + 1);
    await page.getByRole('button', { name: 'Próxima página' }).click();
    await expect(page.locator('[data-session-id]')).toHaveCount(1);
    if (fixture.kind === 'current') {
      await page.getByRole('button', { name: 'Encerrar sessão atual' }).click();
      await expect(
        page.getByRole('heading', { name: 'Encerrar a sessão atual?' }),
      ).toBeVisible();
    } else {
      await page
        .getByRole('button', { name: 'Encerrar todas as sessões', exact: true })
        .click();
      await expect(
        page.getByRole('heading', { name: 'Encerrar todas as sessões?' }),
      ).toBeVisible();
    }
    if (fixture.kind === 'uncertain') {
      dropNextDelete = 'partial-body';
    } else if (fixture.kind === 'native-retry') {
      dropNextDelete = 'before-headers';
    }
    await page
      .getByRole('button', {
        name:
          fixture.kind === 'current'
            ? 'Confirmar encerramento'
            : 'Confirmar encerramento de todas',
        exact: true,
      })
      .click();
    if (fixture.kind === 'uncertain') {
      await expect(
        page.getByRole('heading', { name: 'Encerramento não confirmado' }),
      ).toBeVisible();
      await page
        .getByRole('button', { name: 'Consultar sessões', exact: true })
        .click();
      await expect(
        page.getByRole('heading', { name: 'Sua sessão não está ativa' }),
      ).toBeVisible();
    } else if (fixture.kind === 'native-retry') {
      await expect(
        page.getByRole('heading', {
          name: /Sua sessão não está ativa|Encerramento não confirmado/,
        }),
      ).toBeVisible();
      if (
        await page
          .getByRole('button', { name: 'Consultar sessões', exact: true })
          .isVisible()
      ) {
        await page
          .getByRole('button', { name: 'Consultar sessões', exact: true })
          .click();
      }
      await expect(
        page.getByRole('heading', { name: 'Sua sessão não está ativa' }),
      ).toBeVisible();
    } else {
      await expect(
        page.getByRole('heading', { name: 'Encerramento concluído' }),
      ).toBeVisible();
      assert.equal(
        (await context.cookies(origin)).some(
          (cookie) => cookie.name === '__Host-puxiri_session',
        ),
        false,
      );
    }
    await expect(page.locator('[data-session-id]')).toHaveCount(0);
    if (fixture.kind === 'native-retry') {
      assert.ok(mutations >= before + 2 && mutations <= before + 3);
    } else {
      assert.equal(mutations, before + 2);
    }
    assert.deepEqual(errors, []);
    assert.deepEqual(external, []);
    await context.close();
  }
  assert.equal(dropped, 2);
} finally {
  await browser.close();
  proxy.closeAllConnections();
  await new Promise((resolve) => proxy.close(resolve));
}
