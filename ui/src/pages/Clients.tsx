import { KeyRound, TriangleAlert } from 'lucide-react';
import { useMemo, useState, type KeyboardEvent } from 'react';
import { useConfig, useConnections, useKeys, useModels, useRoutes } from '../app/queries';
import { navigate, useLocation } from '../app/router';
import { CodeBlock, CopyField } from '../components/Code';
import { Button, Callout, Field, PageHead, Skeleton, kindLabel } from '../components/ui';
import { errorMessage } from '../lib/api';
import { CLIENT_ORDER, buildGuide, kindMismatch, resolveGatewayUrls, type ClientId } from '../lib/snippets';
import type { ConnectionKind } from '../lib/types';
import { buildModelOptions } from './Playground';

const PLACEHOLDER_MODEL = 'your-model';

export function ClientsPage() {
  const { search } = useLocation();
  const config = useConfig();
  const keys = useKeys();
  const routes = useRoutes();
  const models = useModels();
  const connections = useConnections();
  const options = useMemo(() => buildModelOptions(routes.data ?? [], models.data ?? [], connections.data ?? []), [routes.data, models.data, connections.data]);
  const params = new URLSearchParams(search);
  const initialClient = (CLIENT_ORDER as string[]).includes(params.get('client') ?? '') ? (params.get('client') as ClientId) : 'codex';
  const [client, setClient] = useState<ClientId>(initialClient);
  const [picked, setPicked] = useState('');
  const urls = useMemo(() => resolveGatewayUrls(config.data, window.location), [config.data]);

  const model = picked || options[0]?.id || PLACEHOLDER_MODEL;
  const selected = options.find((o) => o.id === model);
  const modelKind = useMemo<ConnectionKind | null>(() => {
    const route = routes.data?.find((r) => r.model === model);
    const connId = route ? route.targets[0]?.connection_id : models.data?.find((m) => m.id === model)?.connection_id;
    return connections.data?.find((c) => c.id === connId)?.kind ?? null;
  }, [model, routes.data, models.data, connections.data]);

  const guide = buildGuide(client, { urls, model, modelKind, websocket: !!selected?.websocket });
  const mismatch = kindMismatch(guide.speaks, modelKind);

  const onTabKey = (e: KeyboardEvent, i: number) => {
    const delta = e.key === 'ArrowDown' || e.key === 'ArrowRight' ? 1 : e.key === 'ArrowUp' || e.key === 'ArrowLeft' ? -1 : 0;
    if (!delta) return;
    e.preventDefault();
    const next = CLIENT_ORDER[(i + delta + CLIENT_ORDER.length) % CLIENT_ORDER.length];
    setClient(next);
    document.getElementById(`client-tab-${next}`)?.focus();
  };

  return (
    <>
      <PageHead title="Connect clients" description="Point any coding agent or SDK at one local address. Snippets below are generated from this gateway’s live settings." />

      <section className="card" aria-labelledby="endpoints-title">
        <div className="card-head">
          <h2 id="endpoints-title">Gateway endpoints</h2>
          {config.isError ? <span className="muted small">Using this page’s address ({errorMessage(config.error)})</span> : null}
        </div>
        <div className="card-body endpoints">
          {config.isPending ? (
            [0, 1, 2, 3].map((i) => <Skeleton key={i} h={34} />)
          ) : (
            <>
              <Field label="OpenAI-compatible (Responses, Chat Completions)">
                <CopyField value={urls.openai} label="OpenAI base URL" />
              </Field>
              <Field label="Anthropic Messages">
                <CopyField value={urls.anthropic} label="Anthropic base URL" />
              </Field>
              <Field label="Gemini">
                <CopyField value={urls.gemini} label="Gemini base URL" />
              </Field>
              <Field label="Responses WebSocket">
                <CopyField value={urls.websocket} label="WebSocket URL" />
              </Field>
            </>
          )}
        </div>
      </section>

      {keys.data && keys.data.length === 0 ? (
        <Callout
          tone="warn"
          title="You need a client key"
          action={
            <Button size="sm" variant="primary" icon={KeyRound} onClick={() => navigate('/keys?new=1')}>
              Create key
            </Button>
          }
        >
          Every request to the gateway must carry a Switchyard client key. Snippets read it from <code>$SWITCHYARD_API_KEY</code>.
        </Callout>
      ) : (
        <p className="muted small">
          Snippets read your key from <code>$SWITCHYARD_API_KEY</code>, so nothing secret lands in config files or shell history. Keys are shown once, when you
          create them on the <button type="button" className="link link-button" onClick={() => navigate('/keys')}>API keys</button> page.
        </p>
      )}

      <div className="clients-layout">
        <div role="tablist" aria-label="Clients" aria-orientation="vertical" className="client-tabs">
          {CLIENT_ORDER.map((id, i) => {
            const g = buildGuide(id, { urls, model });
            return (
              <button
                key={id}
                id={`client-tab-${id}`}
                role="tab"
                aria-selected={client === id}
                aria-controls="client-panel"
                tabIndex={client === id ? 0 : -1}
                onClick={() => setClient(id)}
                onKeyDown={(e) => onTabKey(e, i)}
              >
                {g.label}
              </button>
            );
          })}
        </div>
        <section className="card card-pad stack client-panel" id="client-panel" role="tabpanel" aria-labelledby={`client-tab-${client}`}>
          <div className="client-panel-head">
            <div className="stack-sm">
              <h2>{guide.label}</h2>
              <p className="muted">{guide.blurb}</p>
            </div>
            <div className="client-model">
              <label className="field-label" htmlFor="client-model">
                Model in snippets
              </label>
              {options.length ? (
                <select id="client-model" className="select mono" value={model} onChange={(e) => setPicked(e.target.value)}>
                  {options.map((o) => (
                    <option key={o.id} value={o.id}>
                      {o.label}
                    </option>
                  ))}
                </select>
              ) : (
                <span className="muted small">Add a connection to fill in real model names.</span>
              )}
            </div>
          </div>
          {mismatch ? (
            <Callout tone="warn" icon={TriangleAlert}>
              {guide.label} speaks the {guide.speaks === 'anthropic' ? 'Anthropic Messages' : 'OpenAI'} API, but <code>{model}</code> is served by {kindLabel(modelKind ?? '')}. Pick a matching model unless you’ve confirmed the gateway translates between these formats.
            </Callout>
          ) : null}
          {guide.blocks.map((b) => (
            <CodeBlock key={b.title} code={b.code} language={b.language} title={b.title} path={b.path} />
          ))}
          {guide.notes.length ? (
            <ul className="notes">
              {guide.notes.map((n) => (
                <li key={n}>{n}</li>
              ))}
            </ul>
          ) : null}
        </section>
      </div>
    </>
  );
}
