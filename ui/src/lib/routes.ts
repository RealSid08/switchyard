import type { Connection, ModelInfo, Route, RouteTarget } from './types';

export type TargetIssue = 'missing-connection' | 'disabled' | 'model-missing' | null;

export function targetIssue(t: RouteTarget, connections: Connection[]): TargetIssue {
  const c = connections.find((x) => x.id === t.connection_id);
  if (!c) return 'missing-connection';
  if (!c.models.includes(t.model)) return 'model-missing';
  if (!c.enabled) return 'disabled';
  return null;
}

export type RouteHealth = 'ok' | 'degraded' | 'down';

export function routeHealth(r: Route, connections: Connection[]): RouteHealth {
  const issues = r.targets.map((t) => targetIssue(t, connections));
  const healthy = issues.filter((i) => i === null).length;
  if (!healthy) return 'down';
  return healthy < r.targets.length ? 'degraded' : 'ok';
}

export interface DirectModel {
  id: string;
  providers: ModelInfo[];
}

/** Models clients can call without a route, grouped by id (several accounts may serve the same id). */
export function directModels(models: ModelInfo[], routes: Route[]): DirectModel[] {
  const routed = new Set(routes.map((r) => r.model));
  const byId = new Map<string, ModelInfo[]>();
  for (const m of models) {
    if (routed.has(m.id)) continue;
    byId.set(m.id, [...(byId.get(m.id) ?? []), m]);
  }
  return [...byId.entries()].map(([id, providers]) => ({ id, providers })).sort((a, b) => b.providers.length - a.providers.length || a.id.localeCompare(b.id));
}

export interface RouteDraft {
  model: string;
  strategy: Route['strategy'];
  targets: { key: string; connection_id: string; model: string }[];
}

export type RouteErrors = Partial<Record<'model' | 'targets', string>> & { target?: Record<string, string> };

export function validateRoute(d: RouteDraft, opts: { existing: string[]; previous?: string; connections: Connection[] }): RouteErrors {
  const e: RouteErrors = {};
  const name = d.model.trim();
  if (!name) e.model = 'Name the model clients will request.';
  else if (name.length > 200) e.model = 'Keep the name under 200 characters.';
  else if (name !== opts.previous && opts.existing.includes(name)) e.model = 'A route with this name already exists. Edit that route instead.';
  if (!d.targets.length) e.targets = 'Add at least one target.';
  else if (d.targets.length > 20) e.targets = 'A route can have at most 20 targets.';
  const seen = new Set<string>();
  const target: Record<string, string> = {};
  for (const t of d.targets) {
    const c = opts.connections.find((x) => x.id === t.connection_id);
    if (!t.connection_id || !c) target[t.key] = 'Choose a connection.';
    else if (!t.model) target[t.key] = 'Choose a model.';
    else if (!c.models.includes(t.model)) target[t.key] = `${c.name} doesn’t list ${t.model}. Add it to the connection first.`;
    else {
      const k = `${t.connection_id}\u0000${t.model}`;
      if (seen.has(k)) target[t.key] = 'This target is listed twice.';
      seen.add(k);
    }
  }
  if (Object.keys(target).length) e.target = target;
  return e;
}

export function moveItem<T>(list: T[], from: number, to: number): T[] {
  if (to < 0 || to >= list.length || from === to) return list;
  const copy = list.slice();
  const [item] = copy.splice(from, 1);
  copy.splice(to, 0, item);
  return copy;
}
