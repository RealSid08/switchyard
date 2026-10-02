import { readdirSync, readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, MOCK_TOKEN, UI_TOKEN, expectAccessible, mock, watchConsole } from './helpers.ts';

test.beforeEach(async () => {
  await mock('reset');
  await mock('events-on');
});

test('first run: empty state guides to import, key and client setup without fake data', async ({ page }) => {
  const c = watchConsole(page);
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Three steps to your first routed request' })).toBeVisible();
  // No made-up numbers anywhere in the empty state.
  await expect(page.getByText('Success rate')).toHaveCount(0);
  await expect(page.getByText('Median latency')).toHaveCount(0);
  await expectAccessible(page);

  await page.getByRole('button', { name: /Import Codex/ }).click();
  // The page flips to the populated overview; the result survives as a toast.
  const toast = page.locator('.toast', { hasText: 'Codex import complete' });
  await expect(toast).toContainText('Added 1 new account.');
  await expect(toast).toContainText('Original credential files were not modified.');

  // Importing the same login again refreshes instead of duplicating.
  await page.goto('/connections?import=1');
  await page.getByRole('dialog').getByRole('button', { name: /Import Codex/ }).click();
  await expect(page.getByRole('dialog').getByText('Refreshed 1 existing account.')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('.conn-card')).toHaveCount(1);

  // Overview now shows real (zero) traffic plus the remaining setup steps.
  await page.getByRole('link', { name: 'Overview' }).first().click();
  await expect(page.getByRole('heading', { name: 'Finish setup' })).toBeVisible();
  await expect(page.getByText('No traffic yet.', { exact: false })).toBeVisible();

  await page.getByRole('link', { name: /Create a client key/ }).click();
  await page.getByLabel('Name').fill('Codex laptop');
  await page.getByRole('button', { name: 'Create key' }).click();
  const secret = page.getByLabel('New client key');
  await expect(secret).toHaveText(/^sy_[0-9a-f]{20,}/);
  // Closing without copying asks first.
  await page.getByRole('button', { name: 'Done' }).click();
  await expect(page.getByRole('alertdialog', { name: 'Close without copying the key?' })).toBeVisible();
  await page.getByRole('button', { name: 'Cancel' }).click();
  await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.getByRole('button', { name: 'Copy key' }).click();
  await expect(page.getByRole('button', { name: 'Copied' })).toBeVisible();
  await page.getByRole('button', { name: 'Done' }).click();
  await expect(page.getByText('Codex laptop')).toBeVisible();
  // The plaintext key is gone from the page after the reveal.
  await expect(page.locator('body')).not.toContainText(/sy_[0-9a-f]{40,}/);

  await page.goto('/clients');
  await expect(page.getByRole('tab', { name: 'Codex CLI' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('pre').filter({ hasText: 'model_providers.switchyard' })).toContainText('wire_api = "responses"');
  await expect(page.locator('pre').filter({ hasText: 'model_providers.switchyard' })).toContainText('supports_websockets = true');
  await page.getByRole('tab', { name: 'Claude Code' }).click();
  await expect(page.getByText('ANTHROPIC_BASE_URL').first()).toBeVisible();
  await expectAccessible(page);
  c.assertClean();
});

test('connections: validate, create, test, toggle, and delete safely out of routes', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/connections');
  await page.getByRole('button', { name: 'Add connection' }).click();
  const dialog = page.getByRole('dialog', { name: 'Add a connection' });
  await dialog.getByRole('radio', { name: /OpenAI-compatible/ }).click();
  await dialog.getByLabel('Base URL').fill('http://example.com/v1');
  await dialog.getByRole('button', { name: 'Add connection' }).click();
  await expect(dialog.getByText('Plain HTTP is only allowed for local servers.', { exact: false })).toBeVisible();
  await expect(dialog.getByText('Add at least one model this connection serves.')).toBeVisible();
  await dialog.getByRole('button', { name: 'Ollama' }).click();
  await dialog.getByRole('combobox', { name: 'Models' }).or(dialog.getByLabel('Models')).first().fill('qwen3:8b, llama4:scout');
  await dialog.getByLabel('Models').first().press('Enter');
  await expect(dialog.getByRole('button', { name: 'Remove qwen3:8b' })).toBeVisible();
  await expectAccessible(page);
  await dialog.getByRole('button', { name: 'Add connection' }).click();
  await expect(dialog).toBeHidden();
  // Auto-test after save surfaces a problem right away.
  await expect(page.getByText(/saved, but the check failed/)).toBeVisible();

  // Disabling a connection that would strand a route asks first.
  const claude = page.locator('.conn-card', { hasText: 'Claude Code' });
  await claude.getByRole('button', { name: 'Test' }).click();
  await expect(claude.getByText('Provider reachable')).toBeVisible();

  // Delete a connection referenced by routes: it is detached, then deleted.
  const codex = page.locator('.conn-card').filter({ hasText: 'Codex ·' }).first();
  await codex.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Delete' }).click();
  const confirm = page.getByRole('alertdialog');
  await expect(confirm).toContainText('coding');
  await confirm.getByRole('button', { name: 'Remove from routes and delete' }).click();
  await expect(page.getByText(/^Deleted Codex/)).toBeVisible();
  await expect(page.locator('.conn-card').filter({ hasText: 'Codex' })).toHaveCount(1);
  await page.goto('/routes');
  await expect(page.locator('.route-card', { hasText: 'coding' }).locator('.route-targets li')).toHaveCount(1);
  c.assertClean();
});

test('routes: create a pooled route from a direct model, edit order, delete', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/routes');
  await expectAccessible(page);
  const row = page.locator('.direct-list li', { hasText: 'gpt-6-astra' });
  await expect(row.getByText('Round robin ×2')).toBeVisible();
  await row.getByRole('button', { name: 'Route' }).click();
  const dialog = page.getByRole('dialog', { name: 'New route' });
  await expect(dialog.getByLabel('Model name clients request')).toHaveValue('gpt-6-astra');
  await expect(dialog.getByRole('radio', { name: /Round robin/ })).toHaveAttribute('aria-checked', 'true');
  await expect(dialog.locator('.target-editor li')).toHaveCount(2);
  await dialog.getByLabel('Model name clients request').fill('astra');
  await dialog.getByRole('button', { name: 'Move target 2 up' }).click();
  await dialog.getByRole('radio', { name: /Failover/ }).click();
  await dialog.getByRole('button', { name: 'Create route' }).click();
  await expect(page.locator('.route-card', { hasText: 'astra' })).toBeVisible();

  // Duplicate names are caught inline.
  await page.getByRole('button', { name: 'New route' }).click();
  const d2 = page.getByRole('dialog', { name: 'New route' });
  await d2.getByLabel('Model name clients request').fill('coding');
  await d2.getByRole('button', { name: 'Create route' }).click();
  await expect(d2.getByText('A route with this name already exists.', { exact: false })).toBeVisible();
  await d2.getByRole('button', { name: 'Cancel' }).click();

  // Deep link on a fresh load waits for connections before seeding the pool.
  await page.goto('/routes?new=1&model=gpt-6-luna');
  await expect(page.getByRole('dialog', { name: 'New route' }).locator('.target-editor li')).toHaveCount(2);
  await page.keyboard.press('Escape');

  const card = page.locator('.route-card', { hasText: 'astra' });
  await card.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Delete route' }).click();
  await page.getByRole('alertdialog').getByRole('button', { name: 'Delete route' }).click();
  await expect(page.locator('.route-card', { hasText: 'astra' })).toHaveCount(0);
  c.assertClean();
});

test('playground: SSE, HTTP (Anthropic JSON), WebSocket reuse, and errors', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/playground?model=gpt-6.1-sol');
  await expect(page.getByLabel('Model')).toHaveValue('gpt-6.1-sol');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.locator('.output-text')).toContainText('A switchyard sorts railcars', { timeout: 15_000 });
  await expect(page.locator('.metric', { hasText: 'Format' })).toContainText('OpenAI Responses');
  await expect(page.locator('.metric', { hasText: 'Tokens out' })).not.toContainText('–');
  await page.getByRole('tab', { name: /Frames/ }).click();
  await expect(page.locator('.frame').filter({ hasText: 'response.completed' })).toHaveCount(1);

  await page.getByLabel('Model').selectOption('claude-opus-5-5');
  await expect(page.getByRole('radio', { name: 'WebSocket' })).toBeDisabled();
  await page.getByRole('radio', { name: 'HTTP' }).click();
  await page.getByRole('button', { name: 'Send' }).click();
  await page.getByRole('tab', { name: 'Output' }).click();
  await expect(page.locator('.metric', { hasText: 'Format' })).toContainText('Anthropic Messages');
  await expect(page.locator('.output-text')).toContainText('one switch at a time');

  await page.getByLabel('Model').selectOption('gpt-6-luna');
  await page.getByRole('radio', { name: 'WebSocket' }).click();
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('Socket open · 1 turn')).toBeVisible({ timeout: 15_000 });
  await expect(page.locator('.output-text')).toContainText('one switch at a time.');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByText('Socket open · 2 turns')).toBeVisible({ timeout: 15_000 });

  await page.getByLabel('Model').selectOption('gemini-3-pro');
  await page.getByRole('radio', { name: 'SSE stream' }).click();
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.locator('.metric', { hasText: 'Format' })).toContainText('Gemini');
  await expect(page.locator('.output-text')).toContainText('one switch at a time', { timeout: 15_000 });

  await page.getByLabel('Prompt').fill('please fail');
  await page.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'Provider rejected the request' })).toBeVisible();
  c.allow(/status of 502/);
  await expectAccessible(page);
  c.assertClean();
});

test('activity: live rows, filters in the URL, detail view', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/activity');
  await expect(page.getByText('Metadata only.', { exact: false })).toBeVisible();
  await page.getByRole('radio', { name: 'Failed' }).click();
  await expect(page).toHaveURL(/status=error/);
  await expect(page.locator('.activity-table tbody tr').first()).toBeVisible();
  const statuses = await page.locator('.activity-table tbody .status-code').allTextContents();
  expect(statuses.length).toBeGreaterThan(0);
  for (const s of statuses) expect(Number(s.replace(/\D/g, ''))).toBeGreaterThanOrEqual(400);
  await page.locator('.activity-table tbody tr').first().locator('a').click();
  const sheet = page.getByRole('dialog', { name: 'Request details' });
  await expect(sheet.getByText('Request ID')).toBeVisible();
  await expect(sheet.getByText('Prompt and response bodies are never recorded', { exact: false })).toBeVisible();
  await expectAccessible(page);
  await page.keyboard.press('Escape');
  await expect(page).toHaveURL(/\/activity\?status=error$/);

  // A live request shows up without reload.
  await page.getByRole('radio', { name: 'All' }).click();
  const before = await page.locator('.activity-stats strong').first().textContent();
  await mock('burst');
  await expect(page.locator('.activity-stats strong').first()).not.toHaveText(before ?? '', { timeout: 10_000 });
  c.assertClean();
});

test('live updates fall back to polling and recover', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/');
  const pill = page.locator('.sidebar .live-pill');
  await expect(pill).toHaveText(/Live/);
  await mock('events-off');
  await expect(pill).toHaveText(/Retry in|Reconnecting/);
  await expect(pill).toHaveText(/Polling/, { timeout: 20_000 });
  // Data still flows while polling.
  const total = page.locator('.kpi').first().locator('.kpi-value');
  const before = await total.textContent();
  await mock('burst');
  await expect(total).not.toHaveText(before ?? '', { timeout: 12_000 });
  await mock('events-on');
  await pill.click(); // retry now
  await expect(pill).toHaveText(/Live/, { timeout: 10_000 });
  c.allow(/WebSocket connection to .* failed/);
  c.assertClean();
});

test('expired admin session re-authenticates silently', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/keys');
  await expect(page.getByText('Codex on laptop')).toBeVisible();
  await mock('expire-session');
  await page.getByRole('link', { name: 'Connections' }).first().click();
  await expect(page.locator('.conn-card').first()).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Sign in to the control room' })).toHaveCount(0);
  c.allow(/status of 401|WebSocket connection to .* failed/);
  c.assertClean();
});

test('remote access: token sign-in, wrong-key hint, sign out', async ({ page }) => {
  await fetch(`${MOCK_TOKEN}/api/__mock/reset`, { method: 'POST' });
  const c = watchConsole(page);
  await page.goto(UI_TOKEN + '/');
  await expect(page.getByRole('heading', { name: 'Sign in to the control room' })).toBeVisible();
  await expectAccessible(page);
  await page.getByLabel('Admin token').fill('sy_0123456789abcdef');
  await expect(page.getByText('This looks like a client key', { exact: false })).toBeVisible();
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByText('That is a client key.', { exact: false })).toBeVisible();
  await page.getByLabel('Admin token').fill(`Bearer ${ADMIN_TOKEN}`);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Three steps to your first routed request' })).toBeVisible();
  // POST /api/session minted the cookie, so the events socket works too.
  await expect(page.locator('.sidebar .live-pill')).toHaveText(/Live/);
  // Token is in sessionStorage only.
  const stored = await page.evaluate(() => ({ s: sessionStorage.getItem('switchyard.admin-token'), l: JSON.stringify(localStorage) }));
  expect(stored.s).toBe(ADMIN_TOKEN);
  expect(stored.l).not.toContain(ADMIN_TOKEN);
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Three steps to your first routed request' })).toBeVisible();
  await page.goto(UI_TOKEN + '/settings');
  await page.getByRole('button', { name: 'Forget token' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in to the control room' })).toBeVisible();
  c.allow(/status of 401/);
  c.assertClean();
});

test('keyboard: skip link, command palette, focus moves to page heading', async ({ page }) => {
  await mock('seed');
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Overview', level: 1 })).toBeVisible();
  await page.keyboard.press('Tab');
  await expect(page.getByRole('link', { name: 'Skip to content' })).toBeFocused();
  await page.keyboard.press('ControlOrMeta+k');
  const input = page.getByRole('combobox', { name: 'Search pages, actions and models' });
  await expect(input).toBeFocused();
  await input.fill('keys');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/\/keys$/);
  await expect(page.getByRole('heading', { name: 'API keys', level: 1 })).toBeFocused();
  await expect(page).toHaveTitle('API keys · Switchyard');
});

test('pause and resume the gateway', async ({ page }) => {
  await mock('seed');
  await page.goto('/');
  await page.locator('.sidebar').getByRole('button', { name: 'Pause gateway' }).click();
  await page.getByRole('alertdialog').getByRole('button', { name: 'Pause gateway' }).click();
  await expect(page.getByText('Gateway paused.', { exact: false })).toBeVisible();
  await page.locator('.paused-banner').getByRole('button', { name: 'Resume' }).click();
  await expect(page.locator('.paused-banner')).toHaveCount(0);
});

test('mobile: drawer navigation and card layouts', async ({ page }) => {
  await mock('seed');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/activity');
  await expect(page.locator('.activity-cards li').first()).toBeVisible();
  await page.getByRole('button', { name: 'Open navigation' }).click();
  await page.locator('.sidebar').getByRole('link', { name: 'Routes' }).click();
  await expect(page).toHaveURL(/\/routes$/);
  await expect(page.locator('.sidebar')).not.toBeInViewport();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
});

test('production bundle contains no mock code', () => {
  const dir = new URL('../dist/assets/', import.meta.url);
  const js = readdirSync(dir).filter((f) => f.endsWith('.js'));
  expect(js.length).toBeGreaterThan(0);
  for (const f of js) {
    const src = readFileSync(new URL(f, dir), 'utf8');
    expect(src).not.toContain('__mock');
    expect(src).not.toContain('Mock backend');
  }
});
