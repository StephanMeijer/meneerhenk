// Builds the dashboard into dist/, which crates/henk/build.rs embeds in
// the henk binary, served at /dashboard.
import { fileURLToPath } from 'node:url';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import type { ProxyOptions } from 'vite';
import { defineConfig } from 'vitest/config';

// The Henk the dev server forwards the API and sign-in to.
const henk = process.env.HENK_URL ?? 'http://127.0.0.1:8080';

const toHenk: ProxyOptions = {
  target: henk,
  // Henk refuses an action whose Origin is not its own public URL, so the
  // dev server says it is Henk. Only in development.
  configure: (proxy) => {
    proxy.on('proxyReq', (req) => req.setHeader('origin', new URL(henk).origin));
  },
};

export default defineConfig(({ mode }) => ({
  base: '/dashboard/',
  plugins: [svelte()],
  resolve: {
    alias: { $lib: fileURLToPath(new URL('./src/lib', import.meta.url)) },
    // Tests render components in jsdom, as the browser build does.
    ...(mode === 'test' ? { conditions: ['browser'] } : {}),
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // Everything as a file of its own: the page's CSP allows no data: URLs
    // and no inline script or style.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
  },
  server: {
    proxy: {
      '/dashboard/api': toHenk,
      '/dashboard/login': toHenk,
      '/dashboard/auth': toHenk,
      '/dashboard/logout': toHenk,
    },
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts'],
  },
}));
