import { readdirSync, readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, MOCK, MOCK_TOKEN, UI_TOKEN, expectAccessible, mock, watchConsole } from './helpers.ts';

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
  await page.getByRole('button', { name: 'Add API key' }).click();
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
  await page.getByRole('button', { name: 'Sign out of this browser' }).click();
  await expect(page.getByRole('heading', { name: 'You’re signed out' })).toBeVisible();
  expect(await page.evaluate(() => sessionStorage.getItem('switchyard.admin-token'))).toBeNull();
  await page.getByRole('button', { name: 'Sign in with a token' }).click();
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


/* ---------------- Browser sign-in (OAuth) ---------------- */

async function openSignIn(page: import('@playwright/test').Page, provider: 'ChatGPT' | 'Claude') {
  await page.goto('/connections?import=1');
  await page.getByRole('button', { name: `Sign in with ${provider} in the browser` }).click();
  const dialog = page.getByRole('dialog', { name: `Sign in with ${provider}` });
  const link = dialog.getByRole('link', { name: 'Open sign-in page' });
  await expect(link).toBeVisible();
  const href = (await link.getAttribute('href')) ?? '';
  const u = new URL(href);
  return { dialog, link, id: u.searchParams.get('id') ?? '', state: u.searchParams.get('state') ?? '' };
}

async function flowStatus(id: string) {
  const res = await fetch(`${MOCK}/api/oauth/${id}`, { headers: { authorization: `Bearer ${ADMIN_TOKEN}` } });
  return (await res.json()) as { status: string; message?: string };
}

test('oauth: local browser sign-in completes automatically and shows ownership', async ({ page, context }) => {
  const c = watchConsole(page);
  const { dialog, link } = await openSignIn(page, 'ChatGPT');
  await expect(dialog.getByText('Waiting for you to approve')).toBeVisible();
  await expect(dialog.getByText(/expires in [45]:\d\d/)).toBeVisible();
  await expectAccessible(page);
  const [popup] = await Promise.all([context.waitForEvent('page'), link.click()]);
  await popup.waitForLoadState();
  await expect(popup.getByText('Signed in.')).toBeVisible();
  await popup.close();
  await expect(dialog.getByText('New account connected.')).toBeVisible();
  await expect(page.locator('.toast', { hasText: 'Signed in: ChatGPT' })).toBeVisible();
  await dialog.getByRole('button', { name: 'Done' }).click();
  await page.keyboard.press('Escape');
  const card = page.locator('.conn-card', { hasText: 'ChatGPT · you@example.com' });
  await expect(card).toBeVisible();
  await expect(card.locator('.conn-source')).toContainText('Browser sign-in · refreshed by Switchyard');

  // Signing in again with the same account refreshes it rather than duplicating.
  await card.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Sign in again' }).click();
  const again = page.getByRole('dialog', { name: 'Sign in with ChatGPT' });
  const [popup2] = await Promise.all([context.waitForEvent('page'), again.getByRole('link', { name: 'Open sign-in page' }).click()]);
  await popup2.close();
  await expect(again.getByText('Existing account refreshed.')).toBeVisible();
  await again.getByRole('button', { name: 'Done' }).click();
  await expect(page.locator('.conn-card', { hasText: 'ChatGPT · you@example.com' })).toHaveCount(1);
  c.assertClean();
});

test('oauth: remote browser pastes the callback address (wrong state first, then right)', async ({ page }) => {
  const c = watchConsole(page);
  const { dialog, id, state } = await openSignIn(page, 'ChatGPT');
  await dialog.getByText('Signing in from a different computer').click();
  const field = dialog.getByLabel('Callback address');
  await field.fill('localhost:1455/auth/callback');
  await dialog.getByRole('button', { name: 'Finish' }).click();
  await expect(dialog.getByText('Paste the full callback address', { exact: false })).toBeVisible();
  await field.fill(`http://localhost:1455/auth/callback?code=mockcode&state=not-${state}`);
  await dialog.getByRole('button', { name: 'Finish' }).click();
  await expect(dialog.getByText('belongs to a different sign-in', { exact: false })).toBeVisible();
  expect((await flowStatus(id)).status).toBe('pending');
  await field.fill(`http://localhost:1455/auth/callback?code=work-account&state=${state}`);
  await dialog.getByRole('button', { name: 'Finish' }).click();
  await expect(dialog.getByText('ChatGPT · work-account')).toBeVisible();
  c.allow(/status of 400/);
  c.assertClean();
});

test('oauth: Claude accepts code#state', async ({ page }) => {
  const { dialog, state } = await openSignIn(page, 'Claude');
  await dialog.getByText('Signing in from a different computer').click();
  await dialog.getByLabel('Callback address').fill(`team-account#${state}`);
  await dialog.getByRole('button', { name: 'Finish' }).click();
  await expect(dialog.getByText('Claude · team-account')).toBeVisible();
});

test('oauth: expiry offers a fresh start', async ({ page }) => {
  await fetch(`${MOCK}/api/__mock/oauth-ttl?seconds=2`, { method: 'POST' });
  const { dialog } = await openSignIn(page, 'ChatGPT');
  await expect(dialog.getByRole('alert').filter({ hasText: 'Sign-in expired' })).toBeVisible({ timeout: 10_000 });
  await fetch(`${MOCK}/api/__mock/oauth-ttl?seconds=300`, { method: 'POST' });
  await dialog.getByRole('button', { name: 'Try again' }).click();
  await expect(dialog.getByRole('link', { name: 'Open sign-in page' })).toBeVisible();
});

test('oauth: closing the dialog cancels the pending sign-in', async ({ page }) => {
  const { dialog, id } = await openSignIn(page, 'Claude');
  await dialog.getByRole('button', { name: 'Cancel sign-in' }).click();
  await expect(dialog).toBeHidden();
  await expect.poll(async () => (await flowStatus(id)).message).toBe('Sign-in cancelled.');
  // Escape cancels too.
  const second = await openSignIn(page, 'Claude');
  await page.keyboard.press('Escape');
  await expect.poll(async () => (await flowStatus(second.id)).status).toBe('error');
});

test('oauth: busy callback port explains itself and retries', async ({ page }) => {
  await mock('oauth-busy');
  await page.goto('/connections?import=1');
  await page.getByRole('button', { name: 'Sign in with ChatGPT in the browser' }).click();
  const dialog = page.getByRole('dialog', { name: 'Sign in with ChatGPT' });
  await expect(dialog.getByRole('alert')).toContainText('Sign-in port is busy');
  await expect(dialog.getByRole('alert')).toContainText('1455');
  await mock('oauth-free');
  await dialog.getByRole('button', { name: 'Try again' }).click();
  await expect(dialog.getByRole('link', { name: 'Open sign-in page' })).toBeVisible();
});

test('connections show credential source and source-appropriate recovery', async ({ page }) => {
  await mock('seed');
  await page.goto('/connections');
  await expect(page.locator('.conn-card', { hasText: 'Claude Code' }).locator('.conn-source')).toContainText('Claude Code login · follows the CLI login');
  await expect(page.locator('.conn-card', { hasText: 'Gemini' }).locator('.conn-source')).toContainText('API key');
  const native = page.locator('.conn-card').filter({ has: page.locator('.conn-source', { hasText: 'Codex CLI login' }) });
  await native.getByRole('button', { name: /More actions/ }).click();
  await expect(page.getByRole('menuitem', { name: 'Re-import from Codex CLI' })).toBeVisible();
  await expect(page.getByRole('menuitem', { name: 'Use a browser sign-in instead' })).toBeVisible();
  await page.getByRole('menuitem', { name: 'Re-import from Codex CLI' }).click();
  await expect(page.locator('.toast', { hasText: 'Re-imported' })).toBeVisible();
  // Nothing on the page reveals a filesystem path for a credential.
  await expect(page.locator('.conn-list')).not.toContainText('/.codex/');
});

test('local sign out is honest and one click to undo', async ({ page }) => {
  await mock('seed');
  await page.goto('/settings');
  await page.getByRole('button', { name: 'Sign out of this browser' }).click();
  await expect(page.getByRole('heading', { name: 'You’re signed out' })).toBeVisible();
  await expect(page.getByText('reloading the page will also sign you in automatically', { exact: false })).toBeVisible();
  // The old cookie no longer works.
  const status = await page.evaluate(async () => (await fetch('/api/overview', { credentials: 'include' })).status);
  expect(status).toBe(401);
  await page.getByRole('button', { name: 'Sign in again' }).click();
  // Back where you were, with a fresh session.
  await expect(page.getByRole('heading', { name: 'Settings', level: 1 })).toBeVisible();
  await expect(page.getByText('Accepting traffic').first()).toBeVisible();
});

test('gateway outage keeps the last data and recovers without a reload', async ({ page }) => {
  await mock('seed');
  await page.goto('/connections');
  await expect(page.locator('.conn-card').first()).toBeVisible();
  await page.route('**/api/**', (r) => r.abort('connectionrefused'));
  await mock('drop-events');
  await expect(page.getByText('Can’t reach the gateway.', { exact: false })).toBeVisible({ timeout: 25_000 });
  await expect(page.locator('.conn-card').first()).toBeVisible();
  await expect(page.getByText('Couldn’t load connections')).toHaveCount(0);
  await page.unroute('**/api/**');
  await page.getByRole('button', { name: 'Retry now' }).click();
  await expect(page.getByText('Can’t reach the gateway.', { exact: false })).toHaveCount(0, { timeout: 15_000 });
  await expect(page.locator('.sidebar .live-pill')).toHaveText(/Live/, { timeout: 15_000 });
});

/* ---------------- Model discovery, health, attempts, Gemini ---------------- */

test('model discovery: choose from the provider catalog and save', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/connections');
  const card = page.locator('.conn-card', { hasText: 'Claude Code' });
  await card.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Choose models…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Models for Claude Code' });
  await expect(dialog.getByText('8 offered by the provider')).toBeVisible();
  await expectAccessible(page);
  await dialog.getByLabel('Filter models').fill('haiku');
  await dialog.getByRole('checkbox', { name: /claude-haiku-4-5/ }).check();
  await dialog.getByLabel('Filter models').fill('');
  // Removing a model a route depends on warns first.
  await dialog.getByRole('checkbox', { name: /claude-opus-5-5/ }).uncheck();
  await expect(dialog.getByText(/leaves route/)).toContainText('coding');
  await dialog.getByRole('checkbox', { name: /claude-opus-5-5/ }).check();
  await dialog.getByRole('button', { name: 'Save 3 models' }).click();
  await expect(page.locator('.toast', { hasText: 'Saved 3 models' })).toBeVisible();
  await expect(card.locator('.model-chip', { hasText: 'claude-haiku-4-5' })).toBeVisible();
  c.assertClean();
});

test('model discovery: rejected credential is explained without signing out', async ({ page }) => {
  await mock('seed');
  await mock('catalog-reject');
  await page.goto('/connections?models=' + (await firstConnectionId(page, 'Gemini')));
  const dialog = page.getByRole('dialog', { name: 'Models for Gemini' });
  await expect(dialog.getByRole('alert')).toContainText('The provider rejected this account’s credentials');
  await expect(dialog.getByRole('alert')).toContainText('Replace the API key');
  await expect(page.getByRole('heading', { name: 'Sign in to the control room' })).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'You’re signed out' })).toHaveCount(0);
  await mock('catalog-accept');
  await dialog.getByRole('button', { name: 'Retry' }).click();
  await expect(dialog.getByText('Only part of the catalog', { exact: false }).or(dialog.getByText('could only be read in part', { exact: false }))).toBeVisible();
});

async function firstConnectionId(page: import('@playwright/test').Page, name: string) {
  await page.goto('/connections');
  // The page's own requests establish the admin session before we call the API directly.
  await expect(page.locator('.conn-card').first()).toBeVisible();
  return page.evaluate(async (n) => {
    const list = (await (await fetch('/api/connections')).json()) as { id: string; name: string }[];
    return list.find((c) => c.name === n)?.id ?? '';
  }, name);
}

test('model discovery from the edit sheet fills the form', async ({ page }) => {
  await mock('seed');
  await page.goto('/connections');
  const card = page.locator('.conn-card').filter({ has: page.locator('.conn-source', { hasText: 'Codex CLI login' }) });
  await card.getByRole('button', { name: /More actions/ }).click();
  await page.getByRole('menuitem', { name: 'Edit' }).click();
  const sheet = page.getByRole('dialog', { name: /^Edit / });
  await sheet.getByRole('button', { name: 'Browse provider models' }).click();
  const picker = page.getByRole('dialog', { name: /^Models for/ });
  await picker.getByRole('checkbox', { name: /gpt-6-nova/ }).check();
  await picker.getByRole('button', { name: 'Use 4 models' }).click();
  await expect(sheet.getByRole('button', { name: 'Remove gpt-6-nova' })).toBeVisible();
});

test('connection health: limited, cooling down, and expiring CLI logins', async ({ page }) => {
  await mock('seed');
  await page.goto('/connections');
  const oauth = page.locator('.conn-card').filter({ has: page.locator('.conn-source', { hasText: 'Browser sign-in' }) });
  await expect(oauth.locator('.health-chip')).toHaveText(/Limited/);
  await expect(oauth.locator('.cooldowns')).toContainText('gpt-6.1-sol');
  await expect(oauth.locator('.cooldowns')).toContainText(/back in\s*1m/);
  const cli = page.locator('.conn-card').filter({ has: page.locator('.conn-source', { hasText: 'Codex CLI login' }) });
  await expect(cli.locator('.conn-expiry')).toContainText('Login expires in 5 h');
  await expect(page.locator('.conn-card', { hasText: 'Gemini' }).locator('.health-chip')).toHaveText(/Ready/);
  await expect(page.locator('.conn-card', { hasText: 'Ollama' }).locator('.health-chip')).toHaveText(/Disabled/);
  await mock('cooldown');
  await page.reload();
  await expect(oauth.locator('.health-chip')).toHaveText(/Cooling down/);
  await expect(oauth.locator('.cooldowns')).toContainText('All models');
  // Routes show the benched target too.
  await page.goto('/routes');
  await expect(page.locator('.route-card', { hasText: 'coding' })).toContainText('account cooling');
});

test('activity: failovers, attempts and gateway timing', async ({ page }) => {
  await mock('seed');
  const c = watchConsole(page);
  await page.goto('/activity');
  await page.getByRole('button', { name: 'Retried' }).click();
  await expect(page).toHaveURL(/retried=1/);
  const rows = page.locator('.activity-table tbody tr');
  await expect(rows.first()).toBeVisible();
  expect(await rows.count()).toBe(await page.locator('.activity-table tbody .retried-mark').count());
  await rows.first().locator('a').click();
  const sheet = page.getByRole('dialog', { name: 'Request details' });
  await expect(sheet.getByRole('region', { name: 'Upstream attempts' })).toBeVisible();
  await expect(sheet.getByRole('region', { name: 'Upstream attempts' })).toContainText('failover');
  await expect(sheet.getByRole('region', { name: 'Upstream attempts' })).toContainText(/Rate limited|Provider unavailable/);
  await expect(sheet.getByRole('region', { name: 'Gateway timing' })).toContainText('Measured by the gateway');
  await expect(sheet.getByText('route', { exact: true })).toBeVisible();
  await expectAccessible(page);
  c.assertClean();
});

test('activity: deep links fall back to the gateway, and expired records say so', async ({ page }) => {
  await mock('seed');
  const id = await page.goto('/').then(() =>
    page.evaluate(async () => ((await (await fetch('/api/requests?limit=500')).json()) as { id: string }[]).at(-1)!.id),
  );
  // Pretend the list window doesn't include it.
  await page.route('**/api/requests?*', (r) => r.fulfill({ status: 200, contentType: 'application/json', body: '[]' }));
  await page.goto(`/activity/${id}`);
  await expect(page.getByRole('dialog', { name: 'Request details' }).getByText(id)).toBeVisible();
  await page.goto('/activity/does-not-exist');
  await expect(page.getByRole('dialog', { name: 'Request details' }).getByText('Not in the recent log')).toBeVisible();
});

test('clients: Gemini SDK setup uses x-goog-api-key', async ({ page }) => {
  await mock('seed');
  await page.goto('/clients?client=gemini');
  await expect(page.getByRole('tab', { name: 'Gemini' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('pre').filter({ hasText: 'google.genai' })).toContainText('base_url=');
  await expect(page.locator('pre').filter({ hasText: 'streamGenerateContent' })).toContainText('x-goog-api-key');
  await expect(page.getByText('hasn’t been verified against a live Gemini account', { exact: false })).toBeVisible();
});
