import { useQueryClient } from '@tanstack/react-query';
import { ArrowLeft, Download, Eye, EyeOff, Info, ShieldCheck } from 'lucide-react';
import { useState, type FormEvent } from 'react';
import { invalidateUsage } from '../../app/queries';
import { Dialog } from '../../components/Dialog';
import { useToast } from '../../components/feedback';
import { Button, Callout, Field, KindMark } from '../../components/ui';
import { errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import { MONITOR_PROVIDERS, monitorProvider, type MonitorProviderInfo } from '../../lib/usage';
import type { Connection } from '../../lib/types';
import type { MonitorCredential, UsageImportProvider, UsageMonitor } from '../../lib/usageTypes';

const IMPORTABLE: string[] = ['cursor', 'opencode', 'opencode_go', 'codex', 'claude', 'antigravity'];

export function WatchDialog({ open, onClose, editing, connections }: { open: boolean; onClose: () => void; editing: UsageMonitor | null; connections: Connection[] }) {
  return (
    <Dialog
      open={open}
      onClose={onClose}
      width={620}
      title={editing ? `Edit ${editing.name}` : 'Watch an account'}
      description={editing ? 'Update how Switchyard reads this account’s usage.' : 'See an account’s limits and billing without routing traffic through it.'}
    >
      {open ? <WatchForm key={editing?.id ?? 'new'} editing={editing} onDone={onClose} connections={connections} /> : null}
    </Dialog>
  );
}

function WatchForm({ editing, onDone, connections }: { editing: UsageMonitor | null; onDone: () => void; connections: Connection[] }) {
  const qc = useQueryClient();
  const toast = useToast();
  const [provider, setProvider] = useState<MonitorProviderInfo | null>(editing ? (monitorProvider(editing.provider) ?? null) : null);
  const [method, setMethod] = useState<MonitorCredential | null>(editing?.credential_source ?? null);
  const [name, setName] = useState(editing?.name ?? '');
  const [secret, setSecret] = useState('');
  const [show, setShow] = useState(false);
  const [link, setLink] = useState(editing?.connection_id ?? '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (!provider) {
    return (
      <div className="stack">
        <div className="choice-grid two-up" role="radiogroup" aria-label="Provider">
          {MONITOR_PROVIDERS.map((p) => (
            <button
              key={p.id}
              type="button"
              role="radio"
              aria-checked={false}
              className="choice"
              disabled={!p.supported}
              onClick={() => {
                setProvider(p);
                setName(p.label);
                setMethod(p.methods.length === 1 ? p.methods[0].id : null);
              }}
            >
              <KindMark kind={p.kind} size="sm" />
              <span className="choice-text">
                <span className="choice-title">{p.label}</span>
                <span className="choice-desc">{p.supported ? p.blurb : 'Not supported yet'}</span>
              </span>
            </button>
          ))}
        </div>
        <p className="trust-note">
          <ShieldCheck aria-hidden />
          Watching only reads usage. It never signs you out of an app, rotates its login or sends prompts anywhere.
        </p>
      </div>
    );
  }

  const m = provider.methods.find((x) => x.id === method) ?? null;
  const importable = IMPORTABLE.includes(provider.id) && !editing;

  const runImport = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.importUsage(provider.id as UsageImportProvider);
      invalidateUsage(qc);
      const skipped = r.skipped?.length ? ` ${r.skipped.map((s) => s.message).join(' ')}` : '';
      toast({
        tone: r.imported ? 'ok' : 'info',
        title: r.imported ? `Watching ${r.imported} ${provider.label} ${r.imported === 1 ? 'account' : 'accounts'}` : `Nothing new from ${provider.label}`,
        message: `${r.message}${skipped}`,
      });
      onDone();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!m) return;
    if (!name.trim()) return setError('Give this account a name.');
    if (m.field && !editing && !secret.trim()) return setError(`Paste the ${m.field.label.toLowerCase()}.`);
    setBusy(true);
    setError(null);
    const body = {
      name: name.trim(),
      provider: provider.id,
      credential_source: m.id,
      connection_id: link || null,
      enabled: editing?.enabled ?? true,
      // Omitted on edit keeps the stored secret.
      ...(secret.trim() ? { credential: secret.trim() } : {}),
    };
    try {
      if (editing) await api.updateMonitor(editing.id, body);
      else await api.createMonitor(body);
      setSecret('');
      invalidateUsage(qc);
      toast({ tone: 'ok', title: editing ? `Updated ${name.trim()}` : `Watching ${name.trim()}`, message: 'The first numbers arrive within a minute.' });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const linkable = connections.filter((c) => c.kind === provider.kind || (provider.id === 'claude' && c.kind === 'anthropic'));

  return (
    <form className="stack" onSubmit={save} noValidate>
      <div className="row">
        {!editing ? (
          <Button size="sm" variant="ghost" icon={ArrowLeft} onClick={() => setProvider(null)}>
            All providers
          </Button>
        ) : null}
        <KindMark kind={provider.kind} size="sm" />
        <strong>{provider.label}</strong>
        <span className="muted small">{provider.blurb}</span>
      </div>

      {provider.notes.map((n) => (
        <Callout key={n} tone="info" icon={Info}>
          {n}
        </Callout>
      ))}

      {provider.methods.length > 1 ? (
        <fieldset className="fieldset">
          <legend className="field-label">How should Switchyard read it?</legend>
          <div className="choice-grid two-up" role="radiogroup" aria-label="Method">
            {provider.methods.map((x) => (
              <button key={x.id} type="button" role="radio" aria-checked={method === x.id} className="choice" onClick={() => setMethod(x.id)} disabled={!!editing && x.id !== editing.credential_source && x.id === 'native'}>
                <span className="choice-text">
                  <span className="choice-title">{x.label}</span>
                  <span className="choice-desc">{x.help}</span>
                </span>
              </button>
            ))}
          </div>
        </fieldset>
      ) : m ? (
        <p className="muted small">{m.help}</p>
      ) : null}

      {m?.id === 'native' && importable ? (
        <div className="stack-sm">
          <Button variant="primary" icon={Download} onClick={() => void runImport()} loading={busy}>
            Import from this machine
          </Button>
          <p className="trust-note">
            <ShieldCheck aria-hidden />
            Read-only. The app keeps its login; Switchyard never refreshes, rotates or edits it.
          </p>
        </div>
      ) : m ? (
        <>
          <Field label="Name" htmlFor="mon-name" hint="Shown on the Limits tab. Name it after the person or workspace.">
            <input id="mon-name" className="input" value={name} onChange={(e) => setName(e.target.value)} maxLength={100} autoComplete="off" />
          </Field>
          {m.field ? (
            <Field
              label={m.field.label}
              htmlFor="mon-secret"
              optional={!!editing}
              hint={editing ? (editing.credential_present ? 'Stored. Leave blank to keep it, or paste a new one to replace it.' : 'Nothing stored yet.') : 'Stored on the gateway machine only, never shown again.'}
            >
              <div className="input-group">
                <input
                  id="mon-secret"
                  className="input mono"
                  type={show ? 'text' : 'password'}
                  value={secret}
                  onChange={(e) => setSecret(e.target.value)}
                  placeholder={editing?.credential_present ? '•••••••• (unchanged)' : m.field.placeholder}
                  autoComplete="new-password"
                  autoCapitalize="off"
                  spellCheck={false}
                  data-1p-ignore
                />
                <Button variant="ghost" size="sm" iconOnly icon={show ? EyeOff : Eye} aria-label={show ? 'Hide' : 'Show'} aria-pressed={show} onClick={() => setShow((v) => !v)} />
              </div>
            </Field>
          ) : null}
          {linkable.length ? (
            <Field label="Same account as a connection" htmlFor="mon-link" optional hint="Link it so limits and routing show together and the account isn’t watched twice.">
              <select id="mon-link" className="select" value={link} onChange={(e) => setLink(e.target.value)}>
                <option value="">Not linked</option>
                {linkable.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
            </Field>
          ) : null}
        </>
      ) : null}

      {error ? (
        <Callout tone="err" title="That didn’t work" role="alert">
          {error}
        </Callout>
      ) : null}

      {m && !(m.id === 'native' && importable) ? (
        <div className="form-actions">
          <Button onClick={onDone}>Cancel</Button>
          <Button type="submit" variant="primary" loading={busy}>
            {editing ? 'Save' : 'Start watching'}
          </Button>
        </div>
      ) : null}
    </form>
  );
}
