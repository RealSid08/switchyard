import { defineConfig, devices } from '@playwright/test';

// E2E runs the PRODUCTION bundle (`pnpm test:e2e` builds first, then vite preview) against the dev-only mock backend.
// Two stacks: loopback cookie auth (5188 -> mock 5189) and remote token auth (5190 -> mock 5191).
const UI = 5188;
const MOCK = 5189;
const UI_TOKEN = 5190;
const MOCK_TOKEN = 5191;
const STOP = { signal: 'SIGTERM', timeout: 3_000 } as const;

export default defineConfig({
  testDir: './e2e',
  fullyParallel: false,
  workers: 1,
  timeout: 45_000,
  // Hard ceiling for the whole run so CI can never hang on a stuck test.
  globalTimeout: 8 * 60_000,
  expect: { timeout: 8_000 },
  reporter: [['list']],
  use: {
    baseURL: `http://127.0.0.1:${UI}`,
    trace: 'retain-on-failure',
    // Axe samples colours mid-animation otherwise; the app honours reduced motion.
    reducedMotion: 'reduce',
    ...devices['Desktop Chrome'],
    channel: undefined,
  },
  projects: [
    { name: 'e2e', testMatch: /.*\.spec\.ts/, testIgnore: /screenshots\.spec\.ts/ },
    { name: 'screenshots', testMatch: /screenshots\.spec\.ts/ },
  ],
  // Each server is stopped with SIGTERM (then SIGKILL after 3 s) when tests finish.
  webServer: [
    { command: `node mock/server.ts --port ${MOCK}`, url: `http://127.0.0.1:${MOCK}/healthz`, reuseExistingServer: false, gracefulShutdown: STOP },
    { command: `node mock/server.ts --port ${MOCK_TOKEN}`, env: { MOCK_AUTH: 'token' }, url: `http://127.0.0.1:${MOCK_TOKEN}/healthz`, reuseExistingServer: false, gracefulShutdown: STOP },
    // Run vite directly (not via `pnpm exec`) so Playwright's shutdown signal reaches it.
    {
      command: 'node node_modules/vite/bin/vite.js preview',
      env: { SWITCHYARD_UI_PORT: String(UI), SWITCHYARD_BACKEND: `http://127.0.0.1:${MOCK}` },
      url: `http://127.0.0.1:${UI}`,
      timeout: 120_000,
      reuseExistingServer: false,
      gracefulShutdown: STOP,
    },
    {
      command: 'node node_modules/vite/bin/vite.js preview',
      env: { SWITCHYARD_UI_PORT: String(UI_TOKEN), SWITCHYARD_BACKEND: `http://127.0.0.1:${MOCK_TOKEN}` },
      url: `http://127.0.0.1:${UI_TOKEN}`,
      timeout: 120_000,
      reuseExistingServer: false,
      gracefulShutdown: STOP,
    },
  ],
});

