import { defineConfig, devices } from '@playwright/test';

const e2ePort = Number(process.env.PUXIRI_E2E_PORT ?? '4174');
if (!Number.isInteger(e2ePort) || e2ePort < 1 || e2ePort > 65535) {
  throw new Error('PUXIRI_E2E_PORT must be a valid TCP port');
}

export default defineConfig({
  testDir: './e2e',
  use: {
    baseURL: `http://127.0.0.1:${e2ePort}`,
    trace: 'retain-on-failure',
  },
  projects: [
    { name: 'desktop', use: { ...devices['Desktop Chrome'] } },
    {
      name: 'mobile-dark',
      use: { ...devices['Pixel 7'], colorScheme: 'dark' },
    },
  ],
  webServer: {
    command: 'node scripts/serve-e2e.mjs',
    url: `http://127.0.0.1:${e2ePort}/health/ready`,
    reuseExistingServer: false,
    timeout: 30000,
  },
});
