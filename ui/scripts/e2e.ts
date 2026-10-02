/**
 * Bounded Playwright runner used by `pnpm test:e2e` / `pnpm screenshots`.
 * - Kills the run if it exceeds a hard deadline (default 10 min).
 * - Afterwards verifies every test server port is free; kills and reports
 *   stragglers and fails the run, so a leak can never go unnoticed in CI.
 */
import { spawn, execFileSync } from 'node:child_process';

const PORTS = [5188, 5189, 5190, 5191];
const deadlineMs = Number(process.env.E2E_DEADLINE_MS ?? 10 * 60_000);
const args = ['node_modules/@playwright/test/cli.js', 'test', ...process.argv.slice(2)];

function listeners(): { port: number; pid: number }[] {
  const out: { port: number; pid: number }[] = [];
  for (const port of PORTS) {
    try {
      const pids = execFileSync('lsof', ['-ti', `tcp:${port}`, '-sTCP:LISTEN'], { encoding: 'utf8' }).trim();
      for (const pid of pids.split('\n').filter(Boolean)) out.push({ port, pid: Number(pid) });
    } catch {
      // lsof exits 1 when nothing listens (or isn't installed): treat as free.
    }
  }
  return out;
}

const before = listeners();
if (before.length) {
  console.error(`e2e: ports already in use before the run: ${before.map((l) => `${l.port} (pid ${l.pid})`).join(', ')}`);
  process.exit(1);
}

const child = spawn(process.execPath, args, { stdio: 'inherit' });
const killer = setTimeout(() => {
  console.error(`e2e: exceeded ${deadlineMs / 1000}s, stopping Playwright`);
  child.kill('SIGTERM');
  setTimeout(() => child.kill('SIGKILL'), 5_000).unref();
}, deadlineMs);

const code: number = await new Promise((resolve) => child.on('exit', (c, sig) => resolve(c ?? (sig ? 1 : 0))));
clearTimeout(killer);

// Give gracefully-stopping servers a moment, then check for leaks.
let leaks = listeners();
for (let i = 0; i < 20 && leaks.length; i++) {
  await new Promise((r) => setTimeout(r, 250));
  leaks = listeners();
}
if (leaks.length) {
  console.error(`e2e: leaked test servers: ${leaks.map((l) => `${l.port} (pid ${l.pid})`).join(', ')}; killing them`);
  for (const l of leaks) {
    try {
      process.kill(l.pid, 'SIGKILL');
    } catch {
      // already gone
    }
  }
  process.exit(code || 1);
}
process.exit(code);
