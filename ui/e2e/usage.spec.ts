import { expect, test } from '@playwright/test';
import { expectAccessible, mock, watchConsole } from './helpers.ts';

test.beforeEach(async () => {
  await mock('events-on');
  await mock('reset');
});

test('usage: empty gateway shows real empty states, never invented numbers', async ({ page }) => {
  const c = watchConsole(page);
  await page.goto('/usage');
  await expect(page.getByRole('heading', { name: 'Nothing counted yet' })).toBeVisible();
  await expect(page.getByText('Earlier traffic isn’t backfilled or guessed', { exact: false })).toBeVisible();
  await expect(page.locator('.usage-hero')).toHaveCount(0);
  await expectAccessible(page);
  await page.getByRole('link', { name: 'Plan limits' }).click();
  await expect(page.getByRole('heading', { name: 'No accounts to show limits for' })).toBeVisible();
  await page.getByRole('link', { name: 'Pricing' }).click();
  await expect(page.getByText('No custom prices.', { exact: false })).toBeVisible();
  c.assertClean();
});

test('usage: spend, honest partial data, unpriced and filters', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/usage');
  const hero = page.getByRole('region', { name: 'Cost and tokens' });
  await expect(hero).toContainText('Estimated cost');
  await expect(hero).toContainText('Billed by providers');
  await expect(hero).toContainText('Not reported');
  // Some requests reported no usage: totals are marked as a minimum, explained once.
  await expect(hero.locator('.at-least').first()).toBeVisible();
  await expect(hero).toContainText('didn’t report token usage');
  await expectAccessible(page);

  // Unpriced traffic links to Pricing rather than counting as $0.
  await expect(hero.getByRole('link', { name: /no price yet/ })).toBeVisible();
  await expect(page.locator('.usage-table')).toContainText('Not priced');

  // The ledger started 21 days ago: the 30-day window says so.
  await page.getByRole('radio', { name: '30 days' }).click();
  await expect(page).toHaveURL(/window=30d/);
  await expect(page.getByText('Partial history for this window')).toBeVisible();

  // Selecting a row filters by it; selecting again clears.
  await page.getByRole('tab', { name: /Models/ }).click();
  await page.locator('.usage-table').getByRole('button', { name: 'claude-opus-5-5' }).click();
  await expect(page).toHaveURL(/model=claude-opus-5-5/);
  await expect(page.getByLabel('Model')).toHaveValue('claude-opus-5-5');
  await page.getByRole('button', { name: /Clear filter/ }).click();
  await expect(page).not.toHaveURL(/model=/);

  // Combined filters with no match get their own empty state.
  await page.goto('/usage?provider=gemini&model=claude-opus-5-5');
  await expect(page.getByRole('heading', { name: 'No usage matches these filters' })).toBeVisible();
  await page.getByRole('button', { name: 'Clear filters' }).click();
  await expect(page.locator('.usage-hero')).toBeVisible();

  // Chart switches metrics and keeps a data table for screen readers.
  await page.getByRole('radio', { name: 'Cost' }).click();
  await expect(page.locator('.chart table caption')).toContainText('Estimated cost per');
  c.assertClean();
});

test('usage: app history import, and "Both" never adds possibly-overlapping totals', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/usage?scope=external');
  await expect(page.getByRole('heading', { name: 'No app reports yet' })).toBeVisible();
  const history = page.getByRole('region', { name: /App history/ }).or(page.locator('section', { has: page.getByRole('heading', { name: /App history/ }) }));
  const codexRow = history.locator('li', { hasText: 'Codex CLI' });
  await codexRow.getByRole('button', { name: 'Import' }).click();
  await expect(codexRow).toContainText(/Importing/);
  await expect(codexRow).toContainText(/160 requests/, { timeout: 10_000 });
  // Requests Switchyard already counted are left out of the app's numbers, and the UI says so.
  await expect(codexRow).toContainText('12 already counted through Switchyard');
  // App logs don't prove billing or outcome: a separate "Billing unknown" estimate, never API spend,
  // and "Outcome not reported" rather than "none finished" or a success rate.
  const hero = page.getByRole('region', { name: 'Cost and tokens' });
  await expect(hero.locator('.cost-unknown')).toContainText(/Billing unknown\s*\$/);
  await expect(hero.locator('.cost-split > div', { hasText: 'API keys' })).toContainText('–');
  await expect(hero).toContainText('API price equivalent, not money charged');
  await expect(page.locator('.usage-kpis .kpi').first()).toContainText('Outcome not reported');
  await expect(page.locator('.usage-kpis')).not.toContainText(/none finished|succeeded/);
  await expect(page.locator('.usage-table')).toContainText('Not reported');
  await expect(page.locator('.usage-table')).toContainText('billing unknown');
  await page.getByRole('radio', { name: 'Cost' }).click();
  await expect(page.locator('.chart-legend')).toContainText('Billing unknown');
  await expect(page.locator('.chart-legend')).not.toContainText('API keys');
  await expect(history).toContainText('never stores your prompts or outputs');
  // Cursor history needs a watched Cursor account first.
  await expect(history.locator('li', { hasText: 'Cursor' }).getByRole('button', { name: 'Watch Cursor' })).toBeVisible();

  // The server's by_source split is used as is: no extra per-scope requests.
  const sideRequests: string[] = [];
  page.on('request', (r) => {
    if (/\/api\/usage\?.*source=(gateway|external)/.test(r.url())) sideRequests.push(r.url());
  });
  await page.goto('/usage?scope=all');
  await expect(page.getByText('Apps may report requests that also went through Switchyard', { exact: false })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Through Switchyard' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Reported by apps' })).toBeVisible();
  await expect(page.locator('.usage-hero')).toHaveCount(0);
  await expect(page.locator('.usage-table')).toContainText('app report');
  await expect(page.getByText('Share of estimated cost within each source')).toBeVisible();
  // One side charted at a time, never stacked together.
  const chartScope = page.getByRole('combobox', { name: 'Chart scope' });
  await expect(chartScope).toHaveValue('gateway');
  await chartScope.selectOption('external');
  await expect(page.locator('.chart table caption')).toContainText('Tokens per');
  expect(sideRequests).toEqual([]);

  // Clearing removes the imported history.
  await page.goto('/usage?scope=external');
  await history.locator('li', { hasText: 'Codex CLI' }).getByRole('button', { name: 'Remove Codex CLI history' }).click();
  await page.getByRole('alertdialog').getByRole('button', { name: 'Remove history' }).click();
  await expect(page.getByRole('heading', { name: 'No app reports yet' })).toBeVisible();
  c.assertClean();
});

test('limits: freshness, stale data, meters, refresh', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/usage/limits');
  const stale = page.locator('.source-card.status-stale');
  await expect(stale).toContainText('last attempt failed');
  await expect(stale).toContainText('Showing the last good numbers');
  // API-key accounts have no plan to read: a short list, not failing cards.
  const apiOnly = page.getByRole('region', { name: 'API key accounts' });
  await expect(apiOnly).toContainText('Gemini');
  await expect(page.locator('.source-card', { hasText: 'Gemini' })).toHaveCount(0);
  await apiOnly.locator('li', { hasText: 'Gemini' }).getByRole('link', { name: 'See usage' }).click();
  await expect(page.getByRole('combobox', { name: 'Account' })).toHaveValue(/.+/);
  await expect(page.getByRole('combobox', { name: 'Account' }).locator('option:checked')).toHaveText('Gemini');
  await expect(page.getByRole('radio', { name: '30 days' })).toBeChecked();
  await page.goBack();
  const claude = page.locator('.source-card', { hasText: 'Claude Code' });
  await expect(claude.locator('.source-fresh')).toContainText(/Updated .* ago/);
  const opus = claude.getByRole('meter', { name: /Weekly Opus/ });
  await expect(opus).toHaveAttribute('aria-valuenow', '92');
  await expect(claude).toContainText('Resets in');
  await expect(claude).toContainText('On-demand');
  await expectAccessible(page);
  await claude.getByRole('button', { name: /Refresh Claude Code/ }).click();
  await expect(claude.locator('.health-chip')).toHaveText(/Refreshing/);
  await expect(claude.locator('.health-chip')).toHaveText(/Up to date/, { timeout: 10_000 });
  c.assertClean();
});

test('limits: watch Cursor, re-import, credential trouble, pause and remove', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/usage/limits');
  await page.getByRole('button', { name: 'Watch an account' }).click();
  const dialog = page.getByRole('dialog', { name: 'Watch an account' });
  await expect(dialog.getByRole('radio', { name: /Gemini/ })).toBeDisabled();
  await dialog.getByRole('radio', { name: /Cursor/ }).click();
  await expect(dialog).toContainText('can’t route traffic to them');
  await dialog.getByRole('radio', { name: /Cursor on this machine/ }).click();
  await dialog.getByRole('button', { name: 'Import from this machine' }).click();
  await expect(page.locator('.toast', { hasText: 'Watching 1 Cursor account' })).toBeVisible();
  const cursor = page.locator('.source-card', { hasText: 'Cursor on this machine' });
  await expect(cursor).toContainText('Included usage');
  await expect(cursor).toContainText('billing cycle');

  // Importing the same app again doesn't duplicate.
  await page.getByRole('button', { name: 'Watch an account' }).click();
  await dialog.getByRole('radio', { name: /Cursor/ }).click();
  await dialog.getByRole('radio', { name: /Cursor on this machine/ }).click();
  await dialog.getByRole('button', { name: 'Import from this machine' }).click();
  await expect(page.locator('.toast', { hasText: 'Nothing new from Cursor' })).toContainText('Already watching');
  await expect(page.locator('.source-card', { hasText: 'Cursor on this machine' })).toHaveCount(1);

  // Each watched Cursor account gets its own history row.
  await page.goto('/usage?scope=external');
  const cursorHistory = page.locator('.app-history li', { hasText: 'Cursor · Cursor on this machine' });
  await cursorHistory.getByRole('button', { name: 'Import' }).click();
  await expect(cursorHistory).toContainText(/160 requests/, { timeout: 10_000 });
  await page.goto('/usage/limits');

  // A cookie-based monitor whose credential stops working.
  await page.getByRole('button', { name: 'Watch an account' }).click();
  await dialog.getByRole('radio', { name: /Cursor/ }).click();
  await dialog.getByRole('radio', { name: /Session cookie/ }).click();
  await dialog.getByLabel('Name').fill('Cursor work');
  await dialog.getByRole('button', { name: 'Start watching' }).click();
  await expect(dialog.getByRole('alert')).toContainText('Paste the session cookie');
  await dialog.getByLabel('Session cookie').fill('fake-cookie-value');
  await dialog.getByRole('button', { name: 'Start watching' }).click();
  await expect(page.locator('.source-card', { hasText: 'Cursor work' })).toBeVisible();
  await expect(page.locator('body')).not.toContainText('fake-cookie-value');
  await mock('monitor-auth');
  await page.reload();
  const work = page.locator('.source-card', { hasText: 'Cursor work' });
  await expect(work.locator('.health-chip')).toHaveText(/Needs sign-in/);
  await work.getByRole('button', { name: 'Update credential' }).click();
  await expect(page.getByRole('dialog', { name: 'Edit Cursor work' })).toBeVisible();
  await page.keyboard.press('Escape');

  await work.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Pause watching' }).click();
  await expect(work.locator('.health-chip')).toHaveText(/Paused/);
  await work.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Stop watching' }).click();
  await page.getByRole('alertdialog').getByRole('button', { name: 'Stop watching' }).click();
  await expect(page.locator('.source-card', { hasText: 'Cursor work' })).toHaveCount(0);
  c.assertClean();
});

test('pricing: price an unpriced model, validation, and sources', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/usage/pricing');
  await expect(page.getByText('Price list mock-fixture')).toBeVisible();
  const callout = page.getByRole('status').or(page.locator('.callout-warn')).filter({ hasText: 'no price' }).first();
  await expect(callout).toBeVisible();
  await expect(page.locator('.pricing-scope')).toContainText('batch discounts');
  await expect(page.locator('.unpriced-list li').first()).toContainText('last seen');
  // Scheduled prices are listed separately with their start date, not mixed into today's.
  await expect(page.getByRole('region', { name: 'Upcoming price changes table' })).toContainText('gemini-3-pro');
  const model = (await page.locator('.unpriced-list li .mono').first().textContent())!.trim();
  await page.locator('.unpriced-list li', { hasText: model }).getByRole('button', { name: 'Set price' }).click();
  await page.getByLabel(`Input price for ${model}, USD per million tokens`).fill('abc');
  await expect(page.getByText('Prices are numbers like 1.25', { exact: false })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Save prices' })).toBeDisabled();
  await page.getByLabel(`Input price for ${model}, USD per million tokens`).fill('0.20');
  await page.getByLabel(`Output price for ${model}, USD per million tokens`).fill('0.80');
  await page.getByLabel(`Cache write price for ${model}, USD per million tokens`).fill('1');
  await page.getByRole('button', { name: 'Save prices' }).click();
  await expect(page.locator('.toast', { hasText: 'Prices saved' })).toContainText('apply to new usage');
  const saved = page.locator('.rates-table tr', { hasText: model });
  await expect(saved).toContainText('Your price');
  await expect(saved).toContainText('$1.00');
  await expect(page.locator('.unpriced-list li', { hasText: model })).toHaveCount(0);
  // Exact list prices are shown unrounded.
  await expect(page.locator('.rates-table tr', { hasText: 'gpt-6-astra' })).toContainText('$0.125');
  await expectAccessible(page);
  c.assertClean();
});

test('overview: usage glance links to the closest limit', async ({ page }) => {
  await mock('seed');
  await page.goto('/');
  const glance = page.getByRole('region', { name: 'Usage in the last 24 hours' });
  await expect(glance).toContainText('Estimated cost, 24 h');
  await expect(glance).toContainText('Opus');
  await expect(glance).toContainText('92%');
  await glance.getByRole('link', { name: /Closest to a limit/ }).click();
  await expect(page).toHaveURL(/\/usage\/limits/);
});

test('usage: older gateways without usage endpoints degrade clearly', async ({ page }) => {
  await mock('seed');
  await page.route('**/api/usage**', (r) => r.fulfill({ status: 404, contentType: 'application/json', body: '{"error":{"message":"Endpoint not found","type":"gateway_error"}}' }));
  await page.goto('/usage');
  await expect(page.getByText('This gateway doesn’t track usage yet')).toBeVisible();
  await page.goto('/usage/limits');
  await expect(page.getByText('This gateway doesn’t read plan limits yet')).toBeVisible();
  await page.goto('/');
  await expect(page.getByRole('region', { name: 'Usage in the last 24 hours' })).toHaveCount(0);
});

test('usage: mobile layouts have no horizontal overflow', async ({ page }) => {
  await mock('seed');
  await page.setViewportSize({ width: 390, height: 844 });
  for (const path of ['/usage', '/usage/limits', '/usage/pricing']) {
    await page.goto(path);
    await expect(page.locator('h1')).toBeVisible();
    await page.waitForLoadState('networkidle');
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    expect(overflow, path).toBeLessThanOrEqual(0);
  }
});

test('connections: Antigravity and OpenCode sit beside ChatGPT and Claude', async ({ page }) => {
  await mock('seed');
  await page.goto('/connections?import=1');
  await page.getByRole('button', { name: 'Sign in with Antigravity in the browser' }).click();
  const signIn = page.getByRole('dialog', { name: 'Sign in with Google' });
  await expect(signIn.getByRole('link', { name: 'Open sign-in page' })).toBeVisible();
  await signIn.getByText('Signing in from a different computer').click();
  await expect(signIn.getByLabel('Callback address')).toHaveAttribute('placeholder', /localhost:51121/);
  await signIn.getByRole('button', { name: 'Cancel sign-in' }).click();
  await page.getByRole('button', { name: 'Import OpenCode keys' }).click();
  await expect(page.locator('.toast', { hasText: 'OpenCode import complete' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('.conn-card', { hasText: 'OpenCode Go' }).first()).toContainText('OpenCode key');
});
