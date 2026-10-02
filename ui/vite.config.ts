/// <reference types="vitest/config" />
import { defineConfig, type ProxyOptions } from 'vite';
import react from '@vitejs/plugin-react';

// The Rust gateway listens on 127.0.0.1:7410 by default. Point the dev proxy
// elsewhere with SWITCHYARD_PORT (or a full SWITCHYARD_BACKEND URL).
const backend = process.env.SWITCHYARD_BACKEND ?? `http://127.0.0.1:${process.env.SWITCHYARD_PORT ?? '7410'}`;
const uiPort = Number(process.env.SWITCHYARD_UI_PORT ?? 5180);

// Proxy as if the browser were talking to the gateway directly: rewrite Host and
// Origin so same-origin checks and the loopback session cookie behave like prod.
const toGateway: ProxyOptions = {
  target: backend,
  changeOrigin: true,
  ws: true,
  configure(proxy) {
    proxy.on('proxyReq', (req) => {
      if (req.getHeader('origin')) req.setHeader('origin', backend);
    });
    proxy.on('proxyReqWs', (req) => {
      if (req.getHeader('origin')) req.setHeader('origin', backend);
    });
  },
};

const proxy = { '/api': toGateway, '/v1': toGateway, '/v1beta': toGateway };

export default defineConfig(({ command }) => {
  if (command === 'build' && process.env.VITE_SWITCHYARD_MOCK) {
    throw new Error('Refusing to build with VITE_SWITCHYARD_MOCK set: mock mode is dev-only.');
  }
  return {
    base: '/',
    plugins: [react()],
    server: { host: '127.0.0.1', port: uiPort, strictPort: true, proxy },
    preview: { host: '127.0.0.1', port: uiPort, strictPort: true, proxy },
    build: { outDir: 'dist', emptyOutDir: true, target: 'es2022', sourcemap: false },
    test: {
      include: ['src/**/*.test.{ts,tsx}', 'mock/**/*.test.ts'],
      environment: 'node',
      restoreMocks: true,
    },
  };
});
