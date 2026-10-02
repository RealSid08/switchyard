/**
 * `pnpm dev:mock` — run the UI against the in-memory mock backend.
 * Starts the mock on SWITCHYARD_MOCK_PORT (5181) and Vite on SWITCHYARD_UI_PORT
 * (5180), proxying through the same config used against the real gateway.
 * The mock starts EMPTY; pass --seed (or use the Mock panel) for sample data.
 */
import { ADMIN_TOKEN, createMockServer } from '../mock/server.ts';

const mockPort = Number(process.env.SWITCHYARD_MOCK_PORT ?? 5181);
const seed = process.argv.includes('--seed');
const auth = process.env.MOCK_AUTH === 'token' ? 'token' : 'cookie';
const mock = createMockServer({ seed, auth });
await new Promise<void>((resolve) => mock.server.listen(mockPort, '127.0.0.1', resolve));

process.env.SWITCHYARD_BACKEND = `http://127.0.0.1:${mockPort}`;
process.env.VITE_SWITCHYARD_MOCK = '1';

const { createServer } = await import('vite');
const vite = await createServer({ configFile: new URL('../vite.config.ts', import.meta.url).pathname });
await vite.listen();
vite.printUrls();
console.log(`  ➜  Mock:    http://127.0.0.1:${mockPort} (${seed ? 'seeded' : 'empty'}, auth: ${auth})`);
if (auth === 'token') console.log(`  ➜  Admin token: ${ADMIN_TOKEN}`);

const shutdown = async () => {
  await vite.close();
  mock.close();
  process.exit(0);
};
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
