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

    test(`sign-in and recovery · ${vp} · ${theme}`, async ({ page, context }) => {
      const c = watchConsole(page);
      await mock('seed');
      await setup(page, vp, theme);
      await page.goto('/connections?import=1');
      await expect(page.getByRole('dialog', { name: 'Connect an account' })).toBeVisible();
      await shot(page, '30-connect-account', vp, theme, { full: false });

      await page.getByRole('button', { name: 'Sign in with ChatGPT in the browser' }).click();
      const dialog = page.getByRole('dialog', { name: 'Sign in with ChatGPT' });
      await expect(dialog.getByRole('link', { name: 'Open sign-in page' })).toBeVisible();
      await shot(page, '31-signin-pending', vp, theme, { full: false });
      await dialog.getByText('Signing in from a different computer').click();
      await dialog.getByLabel('Callback address').fill('http://localhost:1455/auth/callback?state=abc');
      await dialog.getByRole('button', { name: 'Finish' }).click();
      await shot(page, '32-signin-paste', vp, theme, { full: false });
      const [popup] = await Promise.all([context.waitForEvent('page'), dialog.getByRole('link', { name: 'Open sign-in page' }).click()]);
      await popup.close();
      await expect(dialog.getByText('New account connected.')).toBeVisible();
      await shot(page, '33-signin-complete', vp, theme, { full: false });
      await dialog.getByRole('button', { name: 'Done' }).click();
      await page.keyboard.press('Escape');

      await mock('oauth-busy');
      await page.goto('/connections?signin=claude');
      await expect(page.getByRole('dialog', { name: 'Sign in with Claude' }).getByRole('alert')).toBeVisible();
      await shot(page, '34-signin-busy', vp, theme, { full: false });
      await mock('oauth-free');

      await page.goto('/connections');
      await expect(page.locator('.conn-card').first()).toBeVisible();
      const native = page.locator('.conn-card', { hasText: 'Claude Code' });
      await native.getByRole('button', { name: 'Test' }).click();
      await expect(native.getByText('Provider reachable')).toBeVisible();
      await shot(page, '35-connections-sources', vp, theme, { full: true });

      await page.route('**/api/**', (r) => r.abort('connectionrefused'));
      await mock('drop-events');
      await expect(page.locator('.reconnect-banner')).toBeVisible({ timeout: 25_000 });
      await shot(page, '36-reconnecting', vp, theme, { full: false });
      await page.unroute('**/api/**');
      await mock('events-on');
      await page.reload();

      await page.goto('/settings');
      await page.getByRole('button', { name: 'Sign out of this browser' }).click();
      await expect(page.getByRole('heading', { name: 'You’re signed out' })).toBeVisible();
      await shot(page, '37-signed-out', vp, theme, { full: false });
      c.allow(/status of 40[09]|ERR_CONNECTION_REFUSED|WebSocket connection to .* failed/);
      c.assertClean();
    });

    test(`health, catalogs and failovers · ${vp} · ${theme}`, async ({ page }) => {
      const c = watchConsole(page);
      await mock('seed');
      await mock('cooldown');
      await setup(page, vp, theme);
      await page.goto('/connections');
      await expect(page.locator('.health-chip').first()).toBeVisible();
      await shot(page, '40-connections-health', vp, theme, { full: true });

      const claude = page.locator('.conn-card', { hasText: 'Claude Code' });
      await claude.getByRole('button', { name: /More actions/ }).click();
      await page.getByRole('menuitem', { name: 'Choose models…' }).click();
      const picker = page.getByRole('dialog', { name: 'Models for Claude Code' });
      await expect(picker.getByText('8 offered by the provider')).toBeVisible();
      await picker.getByRole('checkbox', { name: /claude-fable-5-1/ }).check();
      await shot(page, '41-model-picker', vp, theme, { full: false });
      await page.keyboard.press('Escape');

      await mock('catalog-reject');
      const gem = page.locator('.conn-card', { hasText: 'Gemini' });
      await gem.getByRole('button', { name: /More actions/ }).click();
      await page.getByRole('menuitem', { name: 'Choose models…' }).click();
      await expect(page.getByRole('dialog', { name: 'Models for Gemini' }).getByRole('alert')).toBeVisible();
      await shot(page, '42-model-picker-rejected', vp, theme, { full: false });
      await mock('catalog-accept');

      await page.goto('/activity?retried=1');
      const first = page.locator(vp === 'mobile' ? '.activity-cards a' : '.activity-table tbody tr a').first();
      await expect(first).toBeVisible();
      await shot(page, '43-activity-retried', vp, theme, { full: false });
      await first.click();
      await expect(page.getByRole('region', { name: 'Upstream attempts' })).toBeVisible();
      await shot(page, '44-request-attempts', vp, theme, { full: false });

      await page.goto('/clients?client=gemini');
      await expect(page.locator('pre').filter({ hasText: 'google.genai' })).toBeVisible();
      await shot(page, '45-clients-gemini', vp, theme, { full: vp === 'mobile' });
      c.allow(/status of 424/);
      c.assertClean();
    });

    test(`usage, limits and pricing · ${vp} · ${theme}`, async ({ page }) => {
      const c = watchConsole(page);
      await mock('seed');
      await setup(page, vp, theme);
      await page.goto('/usage?window=7d');
      await expect(page.locator('.usage-hero')).toBeVisible();
      await shot(page, '50-usage-spend', vp, theme, { full: true });

      await page.goto('/usage?window=7d&model=claude-opus-5-5');
      await expect(page.locator('.usage-hero')).toBeVisible();
      await shot(page, '51-usage-filtered', vp, theme, { full: false });

      await page.goto('/usage?scope=external');
      const codex = page.locator('.app-history li', { hasText: 'Codex CLI' });
      await codex.getByRole('button', { name: 'Import' }).click();
      await expect(codex).toContainText(/requests/, { timeout: 10_000 });
      await page.reload();
      await expect(page.locator('.usage-hero')).toBeVisible();
      await shot(page, '52-usage-apps', vp, theme, { full: true });

      await page.goto('/usage?scope=all');
      await expect(page.getByRole('region', { name: 'Reported by apps' })).toBeVisible();
      await shot(page, '53-usage-both', vp, theme, { full: true });

      await page.goto('/usage/limits');
      await expect(page.locator('.source-card').first()).toBeVisible();
      await shot(page, '54-limits', vp, theme, { full: true });

      await page.goto('/usage/limits?watch=1');
      await expect(page.getByRole('dialog')).toBeVisible();
      await shot(page, '55-watch-account', vp, theme, { full: false });
      await page.keyboard.press('Escape');

      await page.goto('/usage/pricing');
      await expect(page.locator('.rates-table').first()).toBeVisible();
      await shot(page, '56-pricing', vp, theme, { full: true });
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
