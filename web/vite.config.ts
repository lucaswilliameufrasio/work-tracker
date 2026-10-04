import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [svelte()],
  server: {
    proxy: { '/health': 'http://127.0.0.1:3000' },
  },
  build: { sourcemap: false },
  test: { include: ['src/**/*.test.ts'] },
});
