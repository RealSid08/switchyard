import { AxeBuilder } from '@axe-core/playwright';
import { expect, type Page } from '@playwright/test';

export const MOCK = 'http://127.0.0.1:5189';
export const UI_TOKEN = 'http://127.0.0.1:5190';
export const MOCK_TOKEN = 'http://127.0.0.1:5191';
export const ADMIN_TOKEN = 'sy_admin_mockmockmockmockmockmock';

export async function mock(action: string, base = MOCK) {
  const res = await fetch(`${base}/api/__mock/${action}`, { method: 'POST' });
  if (!res.ok) throw new Error(`mock ${action} failed: ${res.status}`);
}

/** Fail the test on any console error or uncaught exception. */
export function watchConsole(page: Page) {
  const problems: string[] = [];
  page.on('console', (m) => {
    if (m.type() === 'error') problems.push(m.text());
  });
  page.on('pageerror', (e) => problems.push(e.message));
  return {
    problems,
    allow: (re: RegExp) => {
      for (let i = problems.length - 1; i >= 0; i--) if (re.test(problems[i])) problems.splice(i, 1);
    },
    assertClean: () => expect(problems, 'console errors').toEqual([]),
  };
}

export async function expectAccessible(page: Page) {
  const results = await new AxeBuilder({ page }).withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa']).analyze();
  const summary = results.violations.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(' ')).slice(0, 3).join(' | ')}`);
  expect(summary, 'axe violations').toEqual([]);
}
