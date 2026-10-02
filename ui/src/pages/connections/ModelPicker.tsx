import { useQuery } from '@tanstack/react-query';
import { RefreshCw, Search, TriangleAlert } from 'lucide-react';
import { useMemo, useState } from 'react';
import { toInput, useRoutes, useSaveConnection } from '../../app/queries';
import { Dialog } from '../../components/Dialog';
import { useToast } from '../../components/feedback';
import { Badge, Button, Callout, Skeleton } from '../../components/ui';
import { ApiError, errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import { filterCatalog, mergeCatalog } from '../../lib/health';
import type { Connection } from '../../lib/types';

const MAX_MODELS = 100;

/**
 * Choose a connection's models from the provider's own catalog (read-only discovery
 * with the stored credential). `save` writes immediately via PUT; `apply` hands the
 * selection back to an open edit form.
 */
export function ModelPickerDialog({
  connection,
  selected,
  mode,
  onApply,
  onClose,
}: {
  connection: Connection | null;
  selected?: string[];
  mode: 'save' | 'apply';
  onApply?: (models: string[]) => void;
  onClose: () => void;
}) {
  return (
    <Dialog
      open={!!connection}
      onClose={onClose}
      width={600}
      title={connection ? `Models for ${connection.name}` : ''}
      description="Pick from what this account can actually use, straight from the provider."
    >
      {connection ? <Picker connection={connection} initial={selected ?? connection.models} mode={mode} onApply={onApply} onClose={onClose} /> : null}
    </Dialog>
  );
}

function Picker({ connection, initial, mode, onApply, onClose }: { connection: Connection; initial: string[]; mode: 'save' | 'apply'; onApply?: (m: string[]) => void; onClose: () => void }) {
  const catalog = useQuery({
    queryKey: ['catalog', connection.id],
    queryFn: ({ signal }) => api.discoverModels(connection.id, signal),
    retry: false,
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });
  const routes = useRoutes();
  const save = useSaveConnection();
  const toast = useToast();
  const [chosen, setChosen] = useState<Set<string>>(() => new Set(initial));
  const [query, setQuery] = useState('');

  const rows = useMemo(() => mergeCatalog(initial, catalog.data?.models ?? []), [initial, catalog.data]);
  const visible = useMemo(() => filterCatalog(rows, query), [rows, query]);
  const removed = initial.filter((m) => !chosen.has(m));
  const brokenRoutes = (routes.data ?? []).filter((r) => r.targets.some((t) => t.connection_id === connection.id && removed.includes(t.model)));
  const count = chosen.size;
  const tooMany = count > MAX_MODELS;

  const toggle = (id: string) =>
    setChosen((s) => {
      const next = new Set(s);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const ordered = () => {
    // Keep the user's existing order, then append new picks in catalog order.
    const out = initial.filter((m) => chosen.has(m));
    for (const r of rows) if (chosen.has(r.id) && !out.includes(r.id)) out.push(r.id);
    return out;
  };

  const confirm = () => {
    const models = ordered();
    if (mode === 'apply') {
      onApply?.(models);
      onClose();
      return;
    }
    save.mutate(
      { id: connection.id, input: toInput(connection, { models }) },
      {
        onSuccess: () => {
          toast({ tone: 'ok', title: `Saved ${models.length} ${models.length === 1 ? 'model' : 'models'}`, message: connection.name });
          onClose();
        },
        onError: (e) => toast({ tone: 'err', title: 'Couldn’t save models', message: errorMessage(e) }),
      },
    );
  };

  const err = catalog.error;
  const rejected = err instanceof ApiError && err.status === 424;

  return (
    <div className="stack">
      {catalog.isPending ? (
        <div className="stack-sm" role="status" aria-label="Loading the provider’s model list">
          <span className="muted small">Asking the provider for its model list. This can take up to 20 seconds.</span>
          {[0, 1, 2, 3, 4].map((i) => (
            <Skeleton key={i} h={32} />
          ))}
        </div>
      ) : err ? (
        <Callout
          tone={rejected ? 'warn' : 'err'}
          title={rejected ? 'The provider rejected this account’s credentials' : 'Couldn’t load the catalog'}
          role="alert"
          action={
            <Button size="sm" icon={RefreshCw} onClick={() => catalog.refetch()}>
              Retry
            </Button>
          }
        >
          {rejected
            ? connection.credential_source === 'oauth'
              ? 'Sign in again from the connection’s menu, then retry. Your Switchyard session is fine.'
              : connection.credential_source === 'native_codex' || connection.credential_source === 'native_claude'
                ? 'Sign in to the CLI again on the gateway machine and re-import, then retry.'
                : 'Replace the API key from Edit, then retry.'
            : errorMessage(err)}{' '}
          You can still edit model IDs by hand.
        </Callout>
      ) : (
        <>
          <div className="picker-tools">
            <div className="input-group" style={{ flex: 1 }}>
              <Search className="input-icon" aria-hidden />
              <input
                className="input has-icon"
                type="search"
                placeholder="Filter models…"
                aria-label="Filter models"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                data-autofocus
              />
            </div>
            <Button size="sm" variant="ghost" onClick={() => setChosen((s) => new Set([...s, ...visible.map((r) => r.id)]))}>
              Select {query ? 'shown' : 'all'}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setChosen((s) => new Set([...s].filter((id) => !visible.some((r) => r.id === id))))}>
              Clear
            </Button>
          </div>
          <div className="muted small" aria-live="polite">
            {catalog.data?.models.length ?? 0} offered by the provider · <strong className="text-2">{count}</strong> selected
          </div>
          {catalog.data?.truncated ? <Callout tone="warn">{catalog.data.message ?? 'Only part of the catalog could be read.'}</Callout> : null}
          <ul className="picker-list" aria-label="Models">
            {visible.map((r) => (
              <li key={r.id}>
                <label className={`picker-row ${chosen.has(r.id) ? 'on' : ''}`}>
                  <input type="checkbox" checked={chosen.has(r.id)} onChange={() => toggle(r.id)} />
                  <span className="picker-text">
                    <span className="mono picker-id">{r.id}</span>
                    {r.name ? <span className="muted xs">{r.name}</span> : null}
                  </span>
                  {!r.inCatalog ? (
                    <Badge tone="warn" title="Configured on this connection but not in the provider’s list. It may be retired or unavailable to this account.">
                      Not offered
                    </Badge>
                  ) : r.configured ? (
                    <Badge tone="outline">In use</Badge>
                  ) : null}
                </label>
              </li>
            ))}
            {!visible.length ? <li className="muted small picker-empty">No models match “{query}”.</li> : null}
          </ul>
        </>
      )}
      {brokenRoutes.length ? (
        <Callout tone="warn" icon={TriangleAlert}>
          Removing {removed.join(', ')} leaves {brokenRoutes.length === 1 ? 'route' : 'routes'} <strong>{brokenRoutes.map((r) => r.model).join(', ')}</strong> pointing at a model this
          account no longer lists. Update the {brokenRoutes.length === 1 ? 'route' : 'routes'} afterwards.
        </Callout>
      ) : null}
      {tooMany ? <Callout tone="err">A connection can list at most {MAX_MODELS} models. Deselect {count - MAX_MODELS}.</Callout> : null}
      <div className="form-actions">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" onClick={confirm} disabled={!count || tooMany || !!err || catalog.isPending} loading={save.isPending}>
          {mode === 'save' ? `Save ${count} ${count === 1 ? 'model' : 'models'}` : `Use ${count} ${count === 1 ? 'model' : 'models'}`}
        </Button>
      </div>
    </div>
  );
}
