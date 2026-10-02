import { QueryClient, keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ApiError } from '../lib/api';
import { api } from '../lib/client';
import { outcome } from '../lib/format';
import { mergeRequests } from '../lib/requests';
import type { UsageQuery, UsageSources } from '../lib/usageTypes';
import type { ImportSource } from '../lib/types';
import type { ApiKey, Connection, ConnectionInput, Overview, RequestRecord, Route, RouteStrategy, RouteTarget } from '../lib/types';
import { useLiveStatus } from './live';

export const qk = {
  overview: ['overview'] as const,
  connections: ['connections'] as const,
  models: ['models'] as const,
  routes: ['routes'] as const,
  requests: (f: RequestQuery) => ['requests', f] as const,
  keys: ['keys'] as const,
  usage: (q: UsageQuery) => ['usage', q] as const,
  pricing: ['usage-pricing'] as const,
  sources: ['usage-sources'] as const,
  monitors: ['usage-monitors'] as const,
  native: ['usage-native'] as const,
  config: ['config'] as const,
};

export interface RequestQuery {
  status?: 'error';
  model?: string;
}

export function createQueryClient() {
  return new QueryClient({
    defaultOptions: {
      queries: {
        staleTime: 10_000,
        refetchOnWindowFocus: true,
        retry: (count, err) => {
          if (err instanceof ApiError && (err.status === 401 || err.status === 403 || err.isUnsupported)) return false;
          return count < 2;
        },
      },
      mutations: { retry: false },
    },
  });
}

/** Poll only while the live socket is down. */
function usePollInterval(ms = 5000): number | false {
  const { status } = useLiveStatus();
  return status === 'live' ? false : ms;
}

export function useOverview() {
  const refetchInterval = usePollInterval();
  return useQuery({ queryKey: qk.overview, queryFn: () => api.overview(), refetchInterval });
}

/** `health: true` refreshes every 15 s so cooldowns and last-used stay current while visible. */
export function useConnections(opts: { health?: boolean } = {}) {
  return useQuery({ queryKey: qk.connections, queryFn: api.connections, refetchInterval: opts.health ? 15_000 : false });
}

export function useModels() {
  return useQuery({ queryKey: qk.models, queryFn: api.models });
}

export function useRoutes() {
  return useQuery({ queryKey: qk.routes, queryFn: api.routes });
}

export function useConfig() {
  return useQuery({ queryKey: qk.config, queryFn: api.config, staleTime: 60_000 });
}

export function useKeys() {
  return useQuery({ queryKey: qk.keys, queryFn: api.keys });
}

export function useRequests(q: RequestQuery, opts: { live?: boolean } = {}) {
  const poll = usePollInterval();
  return useQuery({
    queryKey: qk.requests(q),
    queryFn: () => api.requests({ limit: 500, status: q.status, model: q.model }),
    refetchInterval: opts.live === false ? false : poll,
    // Changing a server-side filter keeps showing the previous rows (filtered client-side) instead of a skeleton.
    placeholderData: keepPreviousData,
  });
}

/** After anything that changes connections, the derived lists need refreshing too. */
function invalidateTopology(qc: QueryClient) {
  void qc.invalidateQueries({ queryKey: qk.connections });
  void qc.invalidateQueries({ queryKey: qk.models });
  void qc.invalidateQueries({ queryKey: qk.overview });
  void qc.invalidateQueries({ queryKey: qk.routes });
}

export function useSetPaused() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (paused: boolean) => api.setPaused(paused),
    onMutate: async (paused) => {
      await qc.cancelQueries({ queryKey: qk.overview });
      const prev = qc.getQueryData<Overview>(qk.overview);
      if (prev) qc.setQueryData<Overview>(qk.overview, { ...prev, paused });
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.overview, ctx.prev);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: qk.overview }),
  });
}

export function toInput(c: Connection, patch: Partial<ConnectionInput> = {}): ConnectionInput {
  return {
    name: c.name,
    kind: c.kind,
    base_url: c.base_url,
    enabled: c.enabled,
    models: c.models,
    supports_websocket: c.supports_websocket,
    ...patch,
  };
}

export function useToggleConnection() {
  const qc = useQueryClient();
  return useMutation({
    // api_key omitted on purpose: the stored credential is kept.
    mutationFn: ({ conn, enabled }: { conn: Connection; enabled: boolean }) => api.updateConnection(conn.id, toInput(conn, { enabled })),
    onMutate: async ({ conn, enabled }) => {
      await qc.cancelQueries({ queryKey: qk.connections });
      const prev = qc.getQueryData<Connection[]>(qk.connections);
      qc.setQueryData<Connection[]>(qk.connections, (list) => list?.map((c) => (c.id === conn.id ? { ...c, enabled } : c)));
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.connections, ctx.prev);
    },
    onSuccess: (saved) => {
      if (saved?.id) qc.setQueryData<Connection[]>(qk.connections, (list) => list?.map((c) => (c.id === saved.id ? saved : c)));
    },
    onSettled: () => invalidateTopology(qc),
  });
}

export function useSaveConnection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, input }: { id?: string; input: ConnectionInput }) =>
      id ? api.updateConnection(id, input) : api.createConnection(input),
    onSuccess: (saved) => {
      if (!saved?.id) return;
      qc.setQueryData<Connection[]>(qk.connections, (list) => {
        if (!list) return [saved];
        return list.some((c) => c.id === saved.id) ? list.map((c) => (c.id === saved.id ? saved : c)) : [...list, saved];
      });
    },
    onSettled: () => invalidateTopology(qc),
  });
}

export function useDeleteConnection() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.deleteConnection(id),
    onMutate: async (id) => {
      await qc.cancelQueries({ queryKey: qk.connections });
      const prev = qc.getQueryData<Connection[]>(qk.connections);
      qc.setQueryData<Connection[]>(qk.connections, (list) => list?.filter((c) => c.id !== id));
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.connections, ctx.prev);
    },
    onSettled: () => invalidateTopology(qc),
  });
}

export function useTestConnection() {
  return useMutation({ mutationFn: (id: string) => api.testConnection(id) });
}

export function useImport() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ source, path }: { source: ImportSource; path?: string }) => api.importCredentials(source, path),
    onSettled: () => invalidateTopology(qc),
  });
}

export function useSaveRoute() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ model, previousModel, targets, strategy }: { model: string; previousModel?: string; targets: RouteTarget[]; strategy: RouteStrategy }) => {
      const saved = await api.saveRoute(model, { targets, strategy });
      // Renaming is "create the new name, then delete the old one".
      if (previousModel && previousModel !== model) await api.deleteRoute(previousModel);
      return saved;
    },
    onMutate: async ({ model, previousModel, targets, strategy }) => {
      await qc.cancelQueries({ queryKey: qk.routes });
      const prev = qc.getQueryData<Route[]>(qk.routes);
      qc.setQueryData<Route[]>(qk.routes, (list = []) => {
        const next = { model, targets, strategy };
        const without = list.filter((r) => r.model !== model && r.model !== previousModel);
        const idx = list.findIndex((r) => r.model === (previousModel ?? model));
        if (idx === -1) return [...without, next];
        const copy = [...without];
        copy.splice(Math.min(idx, copy.length), 0, next);
        return copy;
      });
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.routes, ctx.prev);
    },
    onSettled: () => {
      void qc.invalidateQueries({ queryKey: qk.routes });
      void qc.invalidateQueries({ queryKey: qk.models });
    },
  });
}

export function useDeleteRoute() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (model: string) => api.deleteRoute(model),
    onMutate: async (model) => {
      await qc.cancelQueries({ queryKey: qk.routes });
      const prev = qc.getQueryData<Route[]>(qk.routes);
      qc.setQueryData<Route[]>(qk.routes, (list) => list?.filter((r) => r.model !== model));
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.routes, ctx.prev);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: qk.routes }),
  });
}

export function useCreateKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.createKey(name),
    onSuccess: (created) => {
      // Cache only the safe fields; the plaintext key never enters the query cache.
      const { id, name, prefix, created_at } = created;
      qc.setQueryData<ApiKey[]>(qk.keys, (list = []) => [...list.filter((k) => k.id !== id), { id, name, prefix, created_at }]);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: qk.keys }),
  });
}

export function useRevokeKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.revokeKey(id),
    onMutate: async (id) => {
      await qc.cancelQueries({ queryKey: qk.keys });
      const prev = qc.getQueryData<ApiKey[]>(qk.keys);
      qc.setQueryData<ApiKey[]>(qk.keys, (list) => list?.filter((k) => k.id !== id));
      return { prev };
    },
    onError: (_e, _v, ctx) => {
      if (ctx?.prev) qc.setQueryData(qk.keys, ctx.prev);
    },
    onSettled: () => qc.invalidateQueries({ queryKey: qk.keys }),
  });
}

/** Apply a pushed request record to every cached request list it belongs in. */
export function applyRequestEvent(qc: QueryClient, record: RequestRecord) {
  for (const [key] of qc.getQueriesData<RequestRecord[]>({ queryKey: ['requests'] })) {
    const filter = (key[1] ?? {}) as RequestQuery;
    if (filter.model && filter.model !== record.model) continue;
    if (filter.status === 'error' && outcome(record) !== 'error') continue;
    qc.setQueryData<RequestRecord[]>(key, (list) => mergeRequests(list, [record]));
  }
  qc.setQueryData<Overview>(qk.overview, (o) =>
    o ? { ...o, recent_requests: mergeRequests(o.recent_requests, [record], Math.max(20, o.recent_requests.length)) } : o,
  );
}

/* ---------- Usage ---------- */

/** Usage report; refreshes every minute while visible, keeps the last report while a new filter loads. */
export function useUsage(q: UsageQuery, opts: { enabled?: boolean } = {}) {
  return useQuery({
    enabled: opts.enabled ?? true,
    queryKey: qk.usage(q),
    queryFn: ({ signal }) => api.usage(q, signal),
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
    retry: (count, err) => !(err instanceof ApiError && (err.status === 400 || err.isUnsupported)) && count < 2,
  });
}

export function usePricing() {
  return useQuery({ queryKey: qk.pricing, queryFn: api.pricing, staleTime: 5 * 60_000, retry: (n, e) => !(e instanceof ApiError && e.isUnsupported) && n < 2 });
}

/** Sources poll faster while a refresh is running so results land without a reload. */
export function useUsageSources() {
  return useQuery({
    queryKey: qk.sources,
    queryFn: api.usageSources,
    refetchInterval: (query) => (query.state.data?.refreshing || query.state.data?.sources.some((s) => s.status === 'refreshing' || s.refreshing) ? 3000 : 60_000),
    retry: (n, e) => !(e instanceof ApiError && e.isUnsupported) && n < 2,
  });
}

export function useMonitors() {
  return useQuery({ queryKey: qk.monitors, queryFn: api.monitors, retry: (n, e) => !(e instanceof ApiError && e.isUnsupported) && n < 2 });
}

export function useRefreshUsage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id?: string) => api.refreshUsage(id),
    onSuccess: (_r, id) => {
      // Optimistically show "Refreshing" until the next poll confirms.
      qc.setQueryData<UsageSources>(qk.sources, (d) =>
        d ? { ...d, refreshing: true, sources: d.sources.map((s) => (!id || s.id === id ? { ...s, refreshing: true } : s)) } : d,
      );
      void qc.invalidateQueries({ queryKey: qk.sources });
    },
  });
}

export function invalidateUsage(qc: QueryClient) {
  void qc.invalidateQueries({ queryKey: ['usage'] });
  void qc.invalidateQueries({ queryKey: qk.sources });
  void qc.invalidateQueries({ queryKey: qk.monitors });
}

/** Native app histories (OpenCode, Codex CLI, Claude Code, Cursor). Polls quickly while an import runs. */
export function useNativeHistory(opts: { enabled?: boolean } = {}) {
  return useQuery({
    enabled: opts.enabled ?? true,
    queryKey: qk.native,
    queryFn: api.nativeHistory,
    refetchInterval: (query) => (query.state.data?.running ? 2000 : 60_000),
    retry: (n, e) => !(e instanceof ApiError && e.isUnsupported) && n < 2,
  });
}
