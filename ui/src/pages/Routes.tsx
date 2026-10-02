import { ArrowDown, ArrowUp, CircleAlert, FlaskConical, Pencil, Plus, RefreshCw, Repeat, ShieldCheck, Timer, Trash2, TriangleAlert, Waypoints, X } from 'lucide-react';
import { useMemo, useState, type FormEvent } from 'react';
import { useConnections, useDeleteRoute, useModels, useRoutes, useSaveRoute } from '../app/queries';
import { navigate, useLocation } from '../app/router';
import { Dialog } from '../components/Dialog';
import { useConfirm, useToast } from '../components/feedback';
import { Menu } from '../components/Menu';
import { Badge, Button, Callout, EmptyState, Field, KindMark, PageHead, Skeleton } from '../components/ui';
import { errorMessage } from '../lib/api';
import { displayNames } from '../lib/connections';
import { formatSeconds } from '../lib/health';
import { directModels, moveItem, routeHealth, targetIssue, validateRoute, type RouteDraft, type RouteErrors } from '../lib/routes';
import type { Connection, Route, RouteStrategy } from '../lib/types';

const STRATEGY: Record<RouteStrategy, { label: string; desc: string; icon: typeof Repeat }> = {
  failover: { label: 'Failover', desc: 'Always try targets in order. Move to the next only when one fails.', icon: ShieldCheck },
  round_robin: { label: 'Round robin', desc: 'Spread requests evenly across targets, e.g. several accounts with the same model.', icon: Repeat },
};

export function RoutesPage() {
  const { search } = useLocation();
  const params = new URLSearchParams(search);
  const routes = useRoutes();
  const connections = useConnections();
  const models = useModels();
  const [editing, setEditing] = useState<Route | null>(null);
  const newOpen = params.get('new') === '1';
  const prefill = params.get('model') ?? '';
  const conns = useMemo(() => connections.data ?? [], [connections.data]);
  const names = useMemo(() => displayNames(conns), [conns]);
  const direct = useMemo(() => directModels(models.data ?? [], routes.data ?? []), [models.data, routes.data]);

  const close = () => {
    setEditing(null);
    if (newOpen) navigate('/routes', { replace: true });
  };

  const loading = routes.isPending || connections.isPending;
  const noConnections = !connections.isPending && conns.length === 0;

  return (
    <>
      <PageHead
        title="Routes"
        description="Give clients one stable model name and decide which accounts serve it, in what order, and how load is shared."
        actions={
          <Button variant="primary" icon={Plus} onClick={() => navigate('/routes?new=1')} disabled={noConnections}>
            New route
          </Button>
        }
      />
      {loading ? (
        <div className="stack" role="status" aria-busy aria-label="Loading routes">
          {[0, 1].map((i) => (
            <div className="card card-pad stack-sm" key={i}>
              <Skeleton w="25%" h={18} />
              <Skeleton w="60%" />
            </div>
          ))}
        </div>
      ) : routes.isError && !routes.data ? (
        <Callout tone="err" title="Couldn’t load routes" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => routes.refetch()}>Retry</Button>}>
          {errorMessage(routes.error)}
        </Callout>
      ) : noConnections ? (
        <div className="card">
          <EmptyState
            icon={Waypoints}
            title="Connect an account first"
            actions={
              <Button variant="primary" icon={Plus} onClick={() => navigate('/connections')}>
                Add a connection
              </Button>
            }
          >
            Routes point a model name at one or more connections. Once you have a connection, its models are callable right away, and routes let you pool or
            fail over between accounts.
          </EmptyState>
        </div>
      ) : (
        <>
          {routes.data?.length ? (
            <ul className="route-list" aria-label="Routes">
              {routes.data.map((r) => (
                <RouteCard key={r.model} route={r} connections={conns} names={names} onEdit={() => setEditing(r)} />
              ))}
            </ul>
          ) : (
            <div className="card">
              <EmptyState icon={Waypoints} title="No routes yet" headingLevel={2} actions={<Button icon={Plus} onClick={() => navigate('/routes?new=1')}>Create a route</Button>}>
                You don’t need one to get started: every model below is already callable. Add a route to pool several accounts, set a failover order, or
                expose a friendly alias like <code>coding</code>.
              </EmptyState>
            </div>
          )}

          <section className="card" aria-labelledby="direct-title">
            <div className="card-head">
              <h2 id="direct-title">
                Direct models <span className="sub">callable by exact name, no route needed</span>
              </h2>
            </div>
            {direct.length ? (
              <ul className="direct-list">
                {direct.map((m) => (
                  <li key={m.id}>
                    <span className="mono truncate direct-id" title={m.id}>
                      {m.id}
                    </span>
                    <span className="direct-providers">
                      {m.providers.map((p) => (
                        <span key={p.connection_id} className="direct-provider" title={names.get(p.connection_id) ?? p.connection_name}>
                          <KindMark kind={p.kind} size="sm" />
                          <span className="truncate">{names.get(p.connection_id) ?? p.connection_name}</span>
                        </span>
                      ))}
                    </span>
                    {m.providers.length > 1 ? <Badge tone="info" icon={Repeat}>Round robin ×{m.providers.length}</Badge> : <span />}
                    <div className="row direct-actions">
                      <Button size="sm" variant="ghost" icon={FlaskConical} aria-label={`Try ${m.id} in the playground`} iconOnly onClick={() => navigate(`/playground?model=${encodeURIComponent(m.id)}`)} />
                      <Button size="sm" variant="ghost" icon={Waypoints} onClick={() => navigate(`/routes?new=1&model=${encodeURIComponent(m.id)}`)}>
                        Route
                      </Button>
                    </div>
                  </li>
                ))}
              </ul>
            ) : (
              <div className="card-body muted small">
                {models.isPending ? 'Loading…' : 'Every enabled model is covered by a route, or no connection is enabled.'}
              </div>
            )}
          </section>
        </>
      )}
      <RouteEditor
        open={newOpen || !!editing}
        onClose={close}
        route={editing}
        prefillModel={prefill}
        connections={conns}
        names={names}
        existing={(routes.data ?? []).map((r) => r.model)}
        ready={!connections.isPending && !routes.isPending}
      />
    </>
  );
}

function RouteCard({ route: r, connections, names, onEdit }: { route: Route; connections: Connection[]; names: Map<string, string>; onEdit: () => void }) {
  const del = useDeleteRoute();
  const confirm = useConfirm();
  const toast = useToast();
  const health = routeHealth(r, connections);
  const S = STRATEGY[r.strategy] ?? STRATEGY.failover;
  const remove = async () => {
    const ok = await confirm({
      title: `Delete route ${r.model}?`,
      message: 'Clients requesting this name will fall back to direct routing if a connection lists the same model ID, otherwise they get an error.',
      confirmLabel: 'Delete route',
      danger: true,
    });
    if (!ok) return;
    del.mutate(r.model, {
      onSuccess: () => toast({ tone: 'ok', title: `Deleted route ${r.model}` }),
      onError: (e) => toast({ tone: 'err', title: 'Couldn’t delete route', message: errorMessage(e) }),
    });
  };
  return (
    <li className="card route-card">
      <div className="route-head">
        <h3 className="mono truncate" title={r.model}>
          {r.model}
        </h3>
        <Badge tone="outline" icon={S.icon}>
          {S.label}
        </Badge>
        {health === 'down' ? (
          <Badge tone="err" icon={CircleAlert}>
            No usable target
          </Badge>
        ) : health === 'degraded' ? (
          <Badge tone="warn" icon={TriangleAlert}>
            Degraded
          </Badge>
        ) : null}
        <span className="spacer" />
        <Button size="sm" variant="ghost" icon={Pencil} onClick={onEdit}>
          Edit
        </Button>
        <Menu
          label={`More actions for ${r.model}`}
          items={[
            { label: 'Try in playground', icon: FlaskConical, onSelect: () => navigate(`/playground?model=${encodeURIComponent(r.model)}`) },
            'separator',
            { label: 'Delete route', icon: Trash2, onSelect: remove, danger: true },
          ]}
        />
      </div>
      <ol className="route-targets" aria-label={`Targets for ${r.model}`}>
        {r.targets.map((t, i) => {
          const issue = targetIssue(t, connections);
          const c = connections.find((x) => x.id === t.connection_id);
          return (
            <li key={`${t.connection_id}-${t.model}-${i}`} className={issue ? 'has-issue' : ''}>
              <span className="target-order" aria-hidden>
                {r.strategy === 'failover' ? i + 1 : '•'}
              </span>
              {c ? <KindMark kind={c.kind} size="sm" /> : <span className="kind-mark sm" aria-hidden />}
              <span className="truncate target-conn">{c ? names.get(c.id) : 'Deleted connection'}</span>
              <span className="muted" aria-hidden>
                →
              </span>
              <span className="mono truncate target-model">{t.model}</span>
              {issue ? (
                <span className="target-issue">
                  <TriangleAlert aria-hidden width={13} height={13} />
                  {issue === 'disabled' ? 'disabled' : issue === 'model-missing' ? 'model removed from connection' : 'connection deleted'}
                </span>
              ) : c ? (
                <TargetCooling connection={c} model={t.model} />
              ) : null}
            </li>
          );
        })}
      </ol>
    </li>
  );
}

/** A target that's temporarily benched: Switchyard skips it until the cooldown ends. */
function TargetCooling({ connection, model }: { connection: Connection; model: string }) {
  const cd = connection.health?.cooldowns.find((x) => x.model === '*' || x.model === model);
  if (!cd || cd.retry_after_seconds <= 0) return null;
  return (
    <span className="target-issue" title="Benched after a rate limit or failure. Requests skip this target until the cooldown ends.">
      <Timer aria-hidden width={13} height={13} />
      {cd.model === '*' ? 'account cooling' : 'cooling'} · {formatSeconds(cd.retry_after_seconds)}
    </span>
  );
}

let keySeq = 0;
const newKey = () => `t${++keySeq}`;

function RouteEditor({
  open,
  onClose,
  route,
  prefillModel,
  connections,
  names,
  existing,
  ready,
}: {
  open: boolean;
  onClose: () => void;
  route: Route | null;
  prefillModel: string;
  connections: Connection[];
  names: Map<string, string>;
  existing: string[];
  ready: boolean;
}) {
  return (
    <Dialog
      open={open}
      onClose={onClose}
      sheet
      title={route ? `Edit route ${route.model}` : 'New route'}
      description="Clients request the route name; Switchyard picks a target using the strategy."
    >
      {/* The form seeds its draft from connections on mount (deep links), so wait for them. */}
      {ready ? (
        <RouteForm route={route} prefillModel={prefillModel} connections={connections} names={names} existing={existing} onDone={onClose} />
      ) : (
        <div className="stack" role="status" aria-label="Loading">
          <Skeleton h={34} />
          <Skeleton h={80} />
          <Skeleton h={120} />
        </div>
      )}
    </Dialog>
  );
}

function RouteForm({
  route,
  prefillModel,
  connections,
  names,
  existing,
  onDone,
}: {
  route: Route | null;
  prefillModel: string;
  connections: Connection[];
  names: Map<string, string>;
  existing: string[];
  onDone: () => void;
}) {
  const save = useSaveRoute();
  const toast = useToast();
  const [draft, setDraft] = useState<RouteDraft>(() => {
    if (route) return { model: route.model, strategy: route.strategy, targets: route.targets.map((t) => ({ ...t, key: newKey() })) };
    // Prefill: every enabled connection that already serves this model (multi-account pooling in one click).
    const serving = prefillModel ? connections.filter((c) => c.enabled && c.models.includes(prefillModel)) : [];
    return {
      model: prefillModel,
      strategy: serving.length > 1 ? 'round_robin' : 'failover',
      targets: serving.length ? serving.map((c) => ({ key: newKey(), connection_id: c.id, model: prefillModel })) : [{ key: newKey(), connection_id: '', model: '' }],
    };
  });
  const [errors, setErrors] = useState<RouteErrors>({});
  const [submitted, setSubmitted] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);
  const allModels = useMemo(() => [...new Set(connections.flatMap((c) => c.models))].sort(), [connections]);

  const set = (next: RouteDraft) => {
    setDraft(next);
    if (submitted) setErrors(validateRoute(next, { existing, previous: route?.model, connections }));
  };
  const setTarget = (key: string, patch: Partial<RouteDraft['targets'][number]>) =>
    set({ ...draft, targets: draft.targets.map((t) => (t.key === key ? { ...t, ...patch } : t)) });

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitted(true);
    setServerError(null);
    const errs = validateRoute(draft, { existing, previous: route?.model, connections });
    setErrors(errs);
    if (errs.model || errs.targets || errs.target) {
      if (errs.model) document.getElementById('route-model')?.focus();
      return;
    }
    try {
      await save.mutateAsync({
        model: draft.model.trim(),
        previousModel: route?.model,
        strategy: draft.strategy,
        targets: draft.targets.map(({ connection_id, model }) => ({ connection_id, model })),
      });
      toast({ tone: 'ok', title: route ? `Saved ${draft.model.trim()}` : `Route ${draft.model.trim()} created` });
      onDone();
    } catch (err) {
      setServerError(errorMessage(err));
    }
  };

  const shadowsDirect = !route && draft.model.trim() && connections.some((c) => c.models.includes(draft.model.trim()));

  return (
    <form className="stack" onSubmit={submit} noValidate>
      <Field
        label="Model name clients request"
        htmlFor="route-model"
        error={errors.model}
        hint={shadowsDirect ? 'This name matches an upstream model ID; the route takes priority over direct routing.' : 'Any name: an upstream ID or an alias like “coding”. Renaming a route updates it in place.'}
      >
        <input
          id="route-model"
          className="input mono"
          value={draft.model}
          onChange={(e) => set({ ...draft, model: e.target.value })}
          list="route-model-options"
          aria-invalid={!!errors.model || undefined}
          aria-describedby={errors.model ? 'route-model-error' : 'route-model-hint'}
          autoComplete="off"
          spellCheck={false}
          maxLength={220}
        />
        <datalist id="route-model-options">
          {allModels.map((m) => (
            <option key={m} value={m} />
          ))}
        </datalist>
      </Field>

      <fieldset className="fieldset">
        <legend className="field-label">Strategy</legend>
        <div className="choice-grid two" role="radiogroup" aria-label="Strategy">
          {(Object.keys(STRATEGY) as RouteStrategy[]).map((s) => {
            const S = STRATEGY[s];
            return (
              <button key={s} type="button" role="radio" aria-checked={draft.strategy === s} className="choice" onClick={() => set({ ...draft, strategy: s })}>
                <S.icon aria-hidden width={16} height={16} className="muted" style={{ marginTop: 2, flex: 'none' }} />
                <span className="choice-text">
                  <span className="choice-title">{S.label}</span>
                  <span className="choice-desc">{S.desc}</span>
                </span>
              </button>
            );
          })}
        </div>
      </fieldset>

      <fieldset className="fieldset">
        <legend className="field-label">Targets {draft.strategy === 'failover' ? <span className="muted small">(tried top to bottom)</span> : null}</legend>
        <ol className="target-editor">
          {draft.targets.map((t, i) => {
            const conn = connections.find((c) => c.id === t.connection_id);
            const err = errors.target?.[t.key];
            const modelOptions = conn ? conn.models : [];
            return (
              <li key={t.key} className={err ? 'has-error' : ''}>
                <span className="target-order" aria-hidden>
                  {i + 1}
                </span>
                <div className="target-fields">
                  <select
                    className="select"
                    aria-label={`Target ${i + 1} connection`}
                    value={t.connection_id}
                    onChange={(e) => {
                      const next = connections.find((c) => c.id === e.target.value);
                      const keepModel = next?.models.includes(t.model) ? t.model : next?.models.includes(draft.model.trim()) ? draft.model.trim() : (next?.models[0] ?? '');
                      setTarget(t.key, { connection_id: e.target.value, model: keepModel });
                    }}
                  >
                    <option value="">Choose connection…</option>
                    {connections.map((c) => (
                      <option key={c.id} value={c.id}>
                        {names.get(c.id)}
                        {c.enabled ? '' : ' (disabled)'}
                      </option>
                    ))}
                  </select>
                  <select
                    className="select mono"
                    aria-label={`Target ${i + 1} upstream model`}
                    value={t.model}
                    disabled={!conn}
                    onChange={(e) => setTarget(t.key, { model: e.target.value })}
                  >
                    <option value="">{conn ? 'Choose model…' : '—'}</option>
                    {t.model && conn && !modelOptions.includes(t.model) ? <option value={t.model}>{t.model} (missing)</option> : null}
                    {modelOptions.map((m) => (
                      <option key={m} value={m}>
                        {m}
                      </option>
                    ))}
                  </select>
                  {err ? (
                    <div className="field-error" role="alert">
                      <CircleAlert aria-hidden />
                      <span>{err}</span>
                    </div>
                  ) : null}
                </div>
                <div className="target-tools">
                  <Button size="sm" variant="ghost" iconOnly icon={ArrowUp} aria-label={`Move target ${i + 1} up`} disabled={i === 0} onClick={() => set({ ...draft, targets: moveItem(draft.targets, i, i - 1) })} />
                  <Button
                    size="sm"
                    variant="ghost"
                    iconOnly
                    icon={ArrowDown}
                    aria-label={`Move target ${i + 1} down`}
                    disabled={i === draft.targets.length - 1}
                    onClick={() => set({ ...draft, targets: moveItem(draft.targets, i, i + 1) })}
                  />
                  <Button size="sm" variant="ghost" iconOnly icon={X} aria-label={`Remove target ${i + 1}`} onClick={() => set({ ...draft, targets: draft.targets.filter((x) => x.key !== t.key) })} />
                </div>
              </li>
            );
          })}
        </ol>
        {errors.targets ? (
          <div className="field-error" role="alert">
            <CircleAlert aria-hidden />
            <span>{errors.targets}</span>
          </div>
        ) : null}
        <div>
          <Button size="sm" icon={Plus} disabled={draft.targets.length >= 20} onClick={() => set({ ...draft, targets: [...draft.targets, { key: newKey(), connection_id: '', model: '' }] })}>
            Add target
          </Button>
        </div>
      </fieldset>

      {serverError ? (
        <Callout tone="err" title="Couldn’t save the route" role="alert">
          {serverError}
        </Callout>
      ) : null}

      <div className="form-actions">
        <Button onClick={onDone}>Cancel</Button>
        <Button type="submit" variant="primary" loading={save.isPending}>
          {route ? 'Save route' : 'Create route'}
        </Button>
      </div>
    </form>
  );
}
