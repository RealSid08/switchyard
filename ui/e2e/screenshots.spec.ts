import { mkdirSync } from 'node:fs';
import { expect, test, type Page } from '@playwright/test';
import { ADMIN_TOKEN, MOCK_TOKEN, UI_TOKEN, mock, watchConsole } from './helpers.ts';

// `pnpm screenshots` -> ui/screenshots/<screen>-<desktop|mobile>-<dark|light>.png
const OUT = new URL('../screenshots/', import.meta.url).pathname;
mkdirSync(OUT, { recursive: true });

const VIEWPORTS = { desktop: { width: 1440, height: 900 }, mobile: { width: 390, height: 844 } } as const;
type Vp = keyof typeof VIEWPORTS;
const THEMES = ['dark', 'light'] as const;

async function shot(page: Page, name: string, vp: Vp, theme: string, opts: { full?: boolean } = {}) {
  await page.waitForTimeout(250);
  await page.screenshot({ path: `${OUT}${name}-${vp}-${theme}.png`, fullPage: opts.full ?? vp === 'mobile' });
}

async function setup(page: Page, vp: Vp, theme: 'dark' | 'light') {
  await page.setViewportSize(VIEWPORTS[vp]);
  await page.emulateMedia({ colorScheme: theme, reducedMotion: 'reduce' });
}

test.beforeEach(async () => {
  await mock('events-on');
  await mock('reset');
});

for (const theme of THEMES) {
  for (const vp of Object.keys(VIEWPORTS) as Vp[]) {
    test(`empty states · ${vp} · ${theme}`, async ({ page }) => {
      const c = watchConsole(page);
      await mock('reset');
      await setup(page, vp, theme);
      await page.goto('/');
      await expect(page.getByRole('heading', { name: 'Three steps to your first routed request' })).toBeVisible();
      await shot(page, '01-overview-empty', vp, theme, { full: true });
      for (const [path, name] of [
        ['/connections', '02-connections-empty'],
        ['/routes', '03-routes-empty'],
        ['/activity', '04-activity-empty'],
        ['/playground', '05-playground-empty'],
        ['/keys', '07-keys-empty'],
      ] as const) {
        await page.goto(path);
        await expect(page.locator('h1')).toBeVisible();
        await page.waitForLoadState('networkidle');
        await shot(page, name, vp, theme);
      }
      c.assertClean();
    });

    test(`populated · ${vp} · ${theme}`, async ({ page }) => {
      const c = watchConsole(page);
      await mock('reset');
      await mock('seed');
      await setup(page, vp, theme);
      for (const [path, name] of [
        ['/', '10-overview'],
        ['/connections', '11-connections'],
        ['/routes', '12-routes'],
        ['/activity', '13-activity'],
        ['/clients', '15-clients'],
        ['/keys', '16-keys'],
        ['/settings', '17-settings'],
      ] as const) {
        await page.goto(path);
        await expect(page.locator('h1')).toBeVisible();
        await page.waitForLoadState('networkidle');
        await shot(page, name, vp, theme, { full: path !== '/activity' });
      }

      // Activity detail
      await page.goto('/activity');
      await page.locator(vp === 'mobile' ? '.activity-cards a' : '.activity-table tbody tr a').first().click();
      await expect(page.getByRole('dialog', { name: 'Request details' })).toBeVisible();
      await shot(page, '13b-activity-detail', vp, theme, { full: false });

      // Playground after a streamed run
      await page.goto('/playground?model=coding');
      await page.getByRole('button', { name: 'Send' }).click();
      await expect(page.locator('.output-text')).toContainText('one switch at a time', { timeout: 15_000 });
      await shot(page, '14-playground-sse', vp, theme, { full: true });
      await page.getByRole('tab', { name: /Frames/ }).click();
      await shot(page, '14b-playground-frames', vp, theme, { full: true });

      // Forms and dialogs
      await page.goto('/connections?new=1&preset=anthropic');
      await expect(page.getByRole('dialog', { name: 'Add a connection' })).toBeVisible();
      await page.getByRole('dialog').getByRole('button', { name: 'Add connection' }).click();
      await shot(page, '11b-connection-form-errors', vp, theme, { full: false });

      await page.goto('/connections?import=1');
      await page.getByRole('dialog').getByRole('button', { name: /Import Codex/ }).click();
      await expect(page.getByRole('dialog').getByText(/new account|existing account/)).toBeVisible();
      await shot(page, '11c-import', vp, theme, { full: false });

      await page.goto('/routes');
      await page.locator('.route-card', { hasText: 'coding' }).getByRole('button', { name: 'Edit' }).click();
      await expect(page.getByRole('dialog', { name: /Edit route/ })).toBeVisible();
      await shot(page, '12b-route-editor', vp, theme, { full: false });

      await page.goto('/keys?new=1');
      await page.getByLabel('Name').fill('Codex on desktop');
      await page.getByRole('button', { name: 'Create key' }).click();
      await expect(page.getByLabel('New client key')).toBeVisible();
      await shot(page, '16b-key-reveal', vp, theme, { full: false });

      await page.goto('/');
      await expect(page.locator('h1')).toBeVisible();
      if (vp === 'desktop') {
        await page.keyboard.press('ControlOrMeta+k');
        await page.getByRole('combobox').fill('gpt');
        await shot(page, '18-command-palette', vp, theme, { full: false });
      }
      c.assertClean();
    });

    test(`gates and banners · ${vp} · ${theme}`, async ({ page }) => {
      await fetch(`${MOCK_TOKEN}/api/__mock/reset`, { method: 'POST' });
      await setup(page, vp, theme);
      await page.goto(UI_TOKEN + '/');
      await expect(page.getByRole('heading', { name: 'Sign in to the control room' })).toBeVisible();
      await page.getByLabel('Admin token').fill('sy_0123456789abcdef');
      await page.getByRole('button', { name: 'Sign in' }).click();
      await expect(page.getByText('That is a client key.', { exact: false })).toBeVisible();
      await shot(page, '20-sign-in', vp, theme, { full: false });
      await page.getByLabel('Admin token').fill(ADMIN_TOKEN);
      await page.getByRole('button', { name: 'Sign in' }).click();
      await expect(page.getByRole('heading', { name: 'Three steps to your first routed request' })).toBeVisible();

      // Unreachable gateway (simulated by failing every API call)
      const fresh = await page.context().newPage();
      await fresh.setViewportSize(VIEWPORTS[vp]);
      await fresh.emulateMedia({ colorScheme: theme, reducedMotion: 'reduce' });
      await fresh.route('**/api/**', (r) => r.abort('connectionrefused'));
      await fresh.goto('/');
      await expect(fresh.getByRole('heading', { name: 'Gateway unreachable' })).toBeVisible();
      await shot(fresh, '21-unreachable', vp, theme, { full: false });
      await fresh.close();

      // Paused banner + polling pill
      await mock('reset');
      await mock('seed');
      await page.goto('/');
      await expect(page.locator('h1')).toBeVisible();
      if (vp === 'mobile') await page.getByRole('button', { name: 'Open navigation' }).click();
      await page.locator('.sidebar').getByRole('button', { name: 'Pause gateway' }).click();
      await page.getByRole('alertdialog').getByRole('button', { name: 'Pause gateway' }).click();
      await expect(page.locator('.paused-banner')).toBeVisible();
      if (vp === 'mobile') await page.keyboard.press('Escape');
      await mock('events-off');
      await expect(page.locator('.live-pill').first()).toHaveText(/Polling/, { timeout: 20_000 });
      await shot(page, '22-paused-polling', vp, theme, { full: false });
      if (vp === 'mobile') {
        await page.getByRole('button', { name: 'Open navigation' }).click();
        await shot(page, '23-mobile-drawer', vp, theme, { full: false });
      }
      await mock('events-on');
    });
  }
}
