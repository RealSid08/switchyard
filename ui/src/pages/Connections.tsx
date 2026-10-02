import { Activity, CircleAlert, CircleCheck, Clock, Download, KeyRound, ListChecks, LogIn, Timer, Pencil, Plug, Plus, RefreshCw, ShieldCheck, Trash2, TriangleAlert, Zap } from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { qk, useConnections, useDeleteConnection, useImport, useRoutes, useTestConnection, useToggleConnection } from '../app/queries';
import { navigate, useLocation, Link } from '../app/router';
import { Dialog } from '../components/Dialog';
import { useConfirm, useToast } from '../components/feedback';
import { Menu } from '../components/Menu';
import { Badge, Button, Callout, KindMark, PageHead, Skeleton, Switch, kindLabel } from '../components/ui';
import { ApiError, errorMessage } from '../lib/api';
import { api } from '../lib/client';
import { displayNames, hostOf, missingKeyWarning, presetFor, routesUsing, strandedRoutes, type ProviderPresetId } from '../lib/connections';
import { formatMs, formatRelative } from '../lib/format';
import type { Connection, ConnectionTestResult, ImportSource, OAuthProvider, Route } from '../lib/types';
import { ConnectionFormDialog } from './connections/ConnectionForm';
import { ConnectOptions } from './connections/ImportPanel';
import { ModelPickerDialog } from './connections/ModelPicker';
import { ATTEMPT_LABELS, activeCooldowns, expiryInfo, formatSeconds, healthInfo } from '../lib/health';
import { SignInDialog } from './connections/SignIn';
import { credentialSourceInfo, oauthProviderFor } from '../lib/oauth';

type TestState = { status: 'running' } | { status: 'done'; result: ConnectionTestResult; at: number } | { status: 'failed'; message: string };

export function ConnectionsPage() {
  const { search } = useLocation();
  const params = new URLSearchParams(search);
  const connections = useConnections({ health: true });
  const fetchedAt = connections.dataUpdatedAt;
  const modelsParam = params.get('models');
  const [pickingId, setPickingId] = useState<string | null>(null);
  const pickId = pickingId ?? modelsParam;
  const picking = pickId ? (connections.data?.find((c) => c.id === pickId) ?? null) : null;
  const closePicker = () => {
    setPickingId(null);
    if (modelsParam) navigate('/connections', { replace: true });
  };
  const routes = useRoutes();
  const [editing, setEditing] = useState<Connection | null>(null);
  const formOpen = params.get('new') === '1' || !!editing;
  const importOpen = params.get('import') === '1';
  const presetParam = params.get('preset') as ProviderPresetId | null;
  const [tests, setTests] = useState<Record<string, TestState>>({});
  const signinParam = params.get('signin');
  const [signInState, setSignIn] = useState<OAuthProvider | null>(null);
  // ?signin=codex|claude deep-links straight into a browser sign-in.
  const signIn = signInState ?? (signinParam === 'codex' || signinParam === 'claude' ? signinParam : null);
  const closeSignIn = () => {
    setSignIn(null);
    if (signinParam) navigate('/connections', { replace: true });
  };

  const closeForm = () => {
    setEditing(null);
    if (params.has('new')) navigate('/connections', { replace: true });
  };

  const list = connections.data;
  const names = useMemo(() => displayNames(list ?? []), [list]);
  const sorted = useMemo(
    () => [...(list ?? [])].sort((a, b) => Number(b.enabled) - Number(a.enabled) || a.kind.localeCompare(b.kind) || a.name.localeCompare(b.name)),
    [list],
  );
  const test = useTestConnection();
  const runTest = (c: Connection) => {
    setTests((t) => ({ ...t, [c.id]: { status: 'running' } }));
    test.mutate(c.id, {
      onSuccess: (result) => setTests((t) => ({ ...t, [c.id]: { status: 'done', result, at: Date.now() } })),
      onError: (e) => setTests((t) => ({ ...t, [c.id]: { status: 'failed', message: errorMessage(e) } })),
    });
  };
  const testAll = () => (list ?? []).filter((c) => c.enabled).forEach(runTest);

  const anyEnabled = (list ?? []).some((c) => c.enabled);

  return (
    <>
      <PageHead
        title="Connections"
        description="Upstream accounts and API providers Switchyard can send traffic to. Add several accounts for the same provider and Switchyard spreads requests across them."
        actions={
          list?.length ? (
            <>
              <Button icon={RefreshCw} onClick={testAll} disabled={!anyEnabled}>
                Test all
              </Button>
              <Button icon={KeyRound} onClick={() => navigate('/connections?new=1')}>
                Add API key
              </Button>
              <Button variant="primary" icon={Plus} onClick={() => navigate('/connections?import=1')}>
                Connect account
              </Button>
            </>
          ) : null
        }
      />

      {connections.isPending ? (
        <div className="conn-list" role="status" aria-busy aria-label="Loading connections">
          {[0, 1, 2].map((i) => (
            <div key={i} className="card conn-card">
              <Skeleton w={28} h={28} />
              <div className="stack-sm" style={{ flex: 1 }}>
                <Skeleton w="30%" />
                <Skeleton w="55%" h={12} />
              </div>
            </div>
          ))}
        </div>
      ) : connections.isError && !connections.data ? (
        <Callout tone="err" title="Couldn’t load connections" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => connections.refetch()}>Retry</Button>}>
          {errorMessage(connections.error)}
        </Callout>
      ) : !list?.length ? (
        <FirstConnection />
      ) : (
        <>
          {!anyEnabled ? (
            <Callout tone="warn" title="Every connection is disabled">
              Clients will get errors until you enable at least one connection.
            </Callout>
          ) : null}
          <ul className="conn-list" aria-label="Connections">
            {sorted.map((c) => (
              <ConnectionRow
                key={c.id}
                connection={c}
                displayName={names.get(c.id) ?? c.name}
                routes={routes.data}
                allConnections={list}
                test={tests[c.id]}
                onTest={() => runTest(c)}
                onEdit={() => setEditing(c)}
                onSignIn={setSignIn}
                onChooseModels={() => setPickingId(c.id)}
                fetchedAt={fetchedAt}
              />
            ))}
          </ul>
          <p className="muted small">
            Models with no route are served by every enabled connection that lists them, rotating round-robin. Use <Link className="link" to="/routes">Routes</Link> to pin order or failover.
          </p>
        </>
      )}

      <SignInDialog provider={signIn} onClose={closeSignIn} />
      <ModelPickerDialog connection={picking} mode="save" onClose={closePicker} />
      <ConnectionFormDialog open={formOpen} onClose={closeForm} connection={editing} initialPreset={presetParam ?? undefined} />
      <Dialog
        open={importOpen}
        onClose={() => navigate('/connections', { replace: true })}
        title="Connect an account"
        description="Sign in, reuse a CLI login, or add an API key. Add as many accounts as you like."
        width={640}
      >
        <ConnectOptions />
      </Dialog>
    </>
  );
}

function FirstConnection() {
  return (
    <div className="card">
      <div className="first-conn">
        <div className="stack-sm">
          <h2>Bring your accounts</h2>
          <p className="muted">Sign in with ChatGPT or Claude, reuse a CLI login, or add an API key. Add as many accounts as you like and route between them.</p>
        </div>
        <ConnectOptions />
      </div>
    </div>
  );
}

function ConnectionRow({
  connection: c,
  displayName,
  routes,
  allConnections,
  test,
  onTest,
  onEdit,
  onSignIn,
  onChooseModels,
  fetchedAt,
}: {
  connection: Connection;
  displayName: string;
  routes: Route[] | undefined;
  allConnections: Connection[];
  test?: TestState;
  onTest: () => void;
  onEdit: () => void;
  onSignIn: (p: OAuthProvider) => void;
  onChooseModels: () => void;
  fetchedAt: number;
}) {
  const rawSource = credentialSourceInfo(c.credential_source);
  // An "api_key" connection without a stored key (typical for local servers) shouldn't claim one.
  const source =
    rawSource && rawSource.owner === 'key' && !c.credential_present
      ? { ...rawSource, label: missingKeyWarning(c) ? 'No API key stored' : 'No key needed', detail: 'No credential is stored for this connection.' }
      : rawSource;
  const oauthProvider = oauthProviderFor(c.kind);
  const reimport = useImport();
  const reimportFrom: ImportSource | null = c.credential_source === 'native_codex' ? 'codex' : c.credential_source === 'native_claude' ? 'claude' : null;
  const runReimport = () => {
    if (!reimportFrom) return;
    reimport.mutate(
      { source: reimportFrom },
      {
        onSuccess: () => toast({ tone: 'ok', title: `Re-imported ${displayName}`, message: 'Picked up the latest CLI login. Original files were not modified.' }),
        onError: (e) => toast({ tone: 'err', title: 'Re-import failed', message: errorMessage(e) }),
      },
    );
  };
  const authFailed = test?.status === 'done' && !test.result.ok && (test.result.status === 401 || test.result.status === 403);
  const toggle = useToggleConnection();
  const toast = useToast();
  const confirm = useConfirm();
  const del = useDeleteConnection();
  const qc = useQueryClient();
  const usedBy = routesUsing(routes, c.id);
  const [expanded, setExpanded] = useState(false);
  const shown = expanded ? c.models : c.models.slice(0, 6);

  const setEnabled = async (enabled: boolean) => {
    if (!enabled) {
      const stranded = strandedRoutes(routes, allConnections, [c.id]);
      if (stranded.length) {
        const ok = await confirm({
          title: `Disable ${displayName}?`,
          message: (
            <>
              {stranded.length === 1 ? 'This route' : 'These routes'} would have no enabled targets and start failing:{' '}
              <strong>{stranded.map((r) => r.model).join(', ')}</strong>.
            </>
          ),
          confirmLabel: 'Disable anyway',
          danger: true,
        });
        if (!ok) return;
      }
    }
    toggle.mutate(
      { conn: c, enabled },
      { onError: (e) => toast({ tone: 'err', title: `Couldn’t ${enabled ? 'enable' : 'disable'} ${displayName}`, message: errorMessage(e) }) },
    );
  };

  const remove = async () => {
    const ok = await confirm({
      title: `Delete ${displayName}?`,
      message: usedBy.length ? (
        <>
          It’s a target in {usedBy.length === 1 ? 'route' : 'routes'} <strong>{usedBy.map((r) => r.model).join(', ')}</strong>. Deleting removes it from{' '}
          {usedBy.length === 1 ? 'that route' : 'those routes'} first; a route left with no targets is deleted too. The stored credential is erased.
        </>
      ) : (
        'Its stored credential is erased from Switchyard. Your original login files are not touched.'
      ),
      confirmLabel: usedBy.length ? 'Remove from routes and delete' : 'Delete connection',
      danger: true,
    });
    if (!ok) return;
    try {
      // Detach from routes first (the gateway refuses to delete referenced connections).
      for (const r of usedBy) {
        const targets = r.targets.filter((t) => t.connection_id !== c.id);
        if (targets.length) await api.saveRoute(r.model, { targets, strategy: r.strategy });
        else await api.deleteRoute(r.model);
      }
      if (usedBy.length) void qc.invalidateQueries({ queryKey: qk.routes });
      await del.mutateAsync(c.id);
      toast({ tone: 'ok', title: `Deleted ${displayName}` });
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        void qc.invalidateQueries({ queryKey: qk.routes });
        toast({ tone: 'err', title: 'Still used by a route', message: 'A route changed while deleting. Check Routes and try again.' });
      } else if (e instanceof ApiError && e.status === 404) {
        toast({ tone: 'info', title: 'Already deleted', message: `${displayName} was removed elsewhere.` });
        void qc.invalidateQueries({ queryKey: qk.connections });
      } else {
        toast({ tone: 'err', title: `Couldn’t delete ${displayName}`, message: errorMessage(e) });
      }
    }
  };

  return (
    <li className={`card conn-card ${c.enabled ? '' : 'is-disabled'}`}>
      <KindMark kind={c.kind} />
      <div className="conn-main">
        <div className="conn-title">
          <h3 className="truncate" title={c.name}>
            {displayName}
          </h3>
          <span className="muted small">{presetFor(c) === 'compatible' && c.kind === 'openai' ? 'OpenAI-compatible' : kindLabel(c.kind)}</span>
          <HealthChip connection={c} />
          {c.supports_websocket ? (
            <Badge tone="info" icon={Zap} title="Supports Responses WebSocket mode">
              WS
            </Badge>
          ) : null}
          {!missingKeyWarning(c) ? null : (
            <Badge tone="warn" icon={TriangleAlert} title="No API key stored for this connection">
              No key
            </Badge>
          )}
          {usedBy.length ? <Badge tone="outline">{usedBy.length === 1 ? '1 route' : `${usedBy.length} routes`}</Badge> : null}
        </div>
        <div className="conn-sub mono truncate" title={c.base_url}>
          {hostOf(c.base_url)}
        </div>
        {source ? (
          <div className={`conn-source owner-${source.owner}`} title={source.detail}>
            {source.owner === 'gateway' ? <ShieldCheck aria-hidden /> : source.owner === 'source' ? <Download aria-hidden /> : <KeyRound aria-hidden />}
            <span>
              {source.label}
              <span className="muted">
                {' · '}
                {source.owner === 'gateway' ? 'refreshed by Switchyard' : source.owner === 'source' ? (c.credential_source === 'cliproxy' ? 'follows its CLIProxyAPI file' : 'follows the CLI login') : c.credential_present ? 'stored key' : 'local server'}
              </span>
            </span>
            <span className="sr-only">. {source.detail}</span>
          </div>
        ) : null}
        <div className="chips conn-models" aria-label={`${c.models.length} models`}>
          {shown.map((m) => (
            <span className="model-chip" key={m} title={m}>
              {m}
            </span>
          ))}
          {c.models.length > 6 ? (
            <button type="button" className="chip-button" onClick={() => setExpanded((e) => !e)} aria-expanded={expanded}>
              {expanded ? 'Show less' : `+${c.models.length - 6} more`}
            </button>
          ) : null}
          {!c.models.length ? <span className="muted small">No models listed</span> : null}
        </div>
        <HealthDetails connection={c} fetchedAt={fetchedAt} />
        <TestResult test={test} />
        {authFailed ? (
          <div className="conn-recover">
            <span className="small">
              {c.credential_source === 'oauth'
                ? 'The provider rejected this sign-in. Signing in again usually fixes it.'
                : reimportFrom
                  ? `This login follows ${reimportFrom === 'codex' ? 'the Codex CLI' : 'Claude Code'}. Sign in there again on the gateway machine, then re-import. Or switch to a browser sign-in that Switchyard keeps fresh.`
                  : c.credential_source === 'api_key'
                    ? 'The provider rejected this key. Paste a new one from Edit.'
                    : 'The provider rejected this credential.'}
            </span>
            <div className="row row-wrap">
              {reimportFrom ? (
                <Button size="sm" icon={Download} onClick={runReimport} loading={reimport.isPending}>
                  Re-import
                </Button>
              ) : null}
              {oauthProvider && c.credential_source !== 'api_key' ? (
                <Button size="sm" variant={c.credential_source === 'oauth' ? 'primary' : 'default'} icon={LogIn} onClick={() => onSignIn(oauthProvider)}>
                  Sign in again
                </Button>
              ) : null}
              {c.credential_source === 'api_key' ? (
                <Button size="sm" icon={Pencil} onClick={onEdit}>
                  Replace key
                </Button>
              ) : null}
            </div>
          </div>
        ) : null}
      </div>
      <div className="conn-actions">
        <Button size="sm" icon={Plug} onClick={onTest} loading={test?.status === 'running'}>
          Test
        </Button>
        <Switch checked={c.enabled} onChange={setEnabled} label={`${c.enabled ? 'Disable' : 'Enable'} ${displayName}`} />
        <Menu
          label={`More actions for ${displayName}`}
          items={[
            { label: 'Edit', icon: Pencil, onSelect: onEdit },
            { label: 'Choose models…', icon: ListChecks, onSelect: onChooseModels },
            ...(c.credential_source === 'oauth' && oauthProvider ? [{ label: 'Sign in again', icon: LogIn, onSelect: () => onSignIn(oauthProvider) }] : []),
            ...(reimportFrom ? [{ label: `Re-import from ${reimportFrom === 'codex' ? 'Codex CLI' : 'Claude Code'}`, icon: Download, onSelect: runReimport }] : []),
            ...(c.credential_source === 'cliproxy' ? [{ label: 'Re-import from CLIProxyAPI…', icon: Download, onSelect: () => navigate('/connections?import=1') }] : []),
            ...(reimportFrom && oauthProvider ? [{ label: 'Use a browser sign-in instead', icon: LogIn, onSelect: () => onSignIn(oauthProvider) }] : []),
            { label: 'Try in playground', icon: Zap, onSelect: () => navigate(`/playground?model=${encodeURIComponent(c.models[0] ?? '')}`), disabled: !c.models.length || !c.enabled },
            { label: 'View activity', icon: Activity, onSelect: () => navigate(`/activity?connection=${encodeURIComponent(c.id)}`) },
            'separator',
            { label: 'Delete', icon: Trash2, onSelect: remove, danger: true },
          ]}
        />
      </div>
    </li>
  );
}

export function HealthChip({ connection: c }: { connection: Connection }) {
  const info = healthInfo(c.health, c.enabled);
  if (!info) return null;
  return (
    <span className={`health-chip tone-${info.tone}`} title={info.help}>
      <span className="dot" aria-hidden />
      {info.label}
      <span className="sr-only">. {info.help}</span>
    </span>
  );
}

/** Cooldowns (live countdown), last use and credential expiry. Quiet when there's nothing to say. */
function HealthDetails({ connection: c, fetchedAt }: { connection: Connection; fetchedAt: number }) {
  const qc = useQueryClient();
  const h = c.health;
  const hasCooldowns = !!h?.cooldowns.length;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!hasCooldowns) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [hasCooldowns]);
  const cooldowns = activeCooldowns(h, fetchedAt || now, now);
  // When the last cooldown runs out, ask the gateway for the fresh state.
  const ended = hasCooldowns && cooldowns.length === 0;
  useEffect(() => {
    if (ended) void qc.invalidateQueries({ queryKey: qk.connections });
  }, [ended, qc]);
  const expiry = expiryInfo(c, now);
  const lastOk = h?.last_status != null && h.last_status < 400 && !h.last_error;
  return (
    <>
      {cooldowns.length ? (
        <ul className="cooldowns" aria-label="Cooldowns">
          {cooldowns.map((cd) => (
            <li key={cd.model} className={cd.model === '*' ? 'account' : ''}>
              <Timer aria-hidden />
              <span className={cd.model === '*' ? '' : 'mono'}>{cd.label}</span>
              <span className="muted">back in</span>
              <span className="num">{formatSeconds(cd.remaining)}</span>
            </li>
          ))}
        </ul>
      ) : null}
      {h?.last_used_at ? (
        <div className="conn-lastused small">
          <span className={`dot ${lastOk ? 'dot-ok' : 'dot-err'}`} aria-hidden />
          <span>
            Last used {formatRelative(h.last_used_at)}
            {h.last_status != null ? <span className="muted"> · {h.last_status === 0 ? 'no response' : `HTTP ${h.last_status}`}</span> : null}
            {h.last_error ? <span className="muted"> · {ATTEMPT_LABELS[h.last_error] ?? h.last_error}</span> : null}
          </span>
        </div>
      ) : h ? (
        <div className="conn-lastused small muted">Not used yet</div>
      ) : null}
      {expiry ? (
        <div className={`conn-expiry tone-${expiry.tone}`}>
          <Clock aria-hidden />
          {expiry.text}
        </div>
      ) : null}
    </>
  );
}

function TestResult({ test }: { test?: TestState }) {
  const [, tick] = useState(0);
  useEffect(() => {
    if (test?.status !== 'done') return;
    const t = window.setInterval(() => tick((n) => n + 1), 15_000);
    return () => window.clearInterval(t);
  }, [test]);
  if (!test) return null;
  if (test.status === 'running') {
    return (
      <div className="conn-test muted small" role="status">
        Checking the provider…
      </div>
    );
  }
  if (test.status === 'failed') {
    return (
      <div className="conn-test err small" role="status">
        <CircleAlert aria-hidden /> {test.message}
      </div>
    );
  }
  const r = test.result;
  return (
    <div className={`conn-test small ${r.ok ? 'ok' : 'err'}`} role="status">
      {r.ok ? <CircleCheck aria-hidden /> : <CircleAlert aria-hidden />}
      <span>
        {r.message}
        {r.status ? <span className="muted"> · HTTP {r.status}</span> : null}
        {r.latency_ms !== null && r.latency_ms !== undefined ? <span className="muted"> · {formatMs(r.latency_ms)}</span> : null}
        <span className="muted"> · {formatRelative(test.at)}</span>
      </span>
    </div>
  );
}
