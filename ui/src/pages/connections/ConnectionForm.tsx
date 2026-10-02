import { Eye, EyeOff, KeyRound, Lock } from 'lucide-react';
import { useMemo, useState, type FormEvent } from 'react';
import { useSaveConnection } from '../../app/queries';
import { Dialog } from '../../components/Dialog';
import { useToast } from '../../components/feedback';
import { TagInput } from '../../components/TagInput';
import { Badge, Button, Callout, Field, KindMark, Switch } from '../../components/ui';
import { ApiError, errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import {
  COMPATIBLE_ENDPOINTS,
  PROVIDER_PRESETS,
  isLoopbackHost,
  normalizeInput,
  presetFor,
  validateConnection,
  type ConnectionErrors,
  type ProviderPresetId,
} from '../../lib/connections';
import type { Connection, ConnectionInput } from '../../lib/types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** Existing connection to edit; omit to create. */
  connection?: Connection | null;
  initialPreset?: ProviderPresetId;
}

export function ConnectionFormDialog({ open, onClose, connection, initialPreset }: Props) {
  const [busy, setBusy] = useState(false);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      sheet
      locked={busy}
      title={connection ? `Edit ${connection.name}` : 'Add a connection'}
      description={connection ? 'Changes apply to new requests immediately.' : 'Connect an API provider. Switchyard stores the key locally and never shows it again.'}
    >
      <ConnectionForm connection={connection ?? null} initialPreset={initialPreset} onDone={onClose} onBusy={setBusy} />
    </Dialog>
  );
}

function ConnectionForm({ connection, initialPreset, onDone, onBusy }: { connection: Connection | null; initialPreset?: ProviderPresetId; onDone: () => void; onBusy: (b: boolean) => void }) {
  const editing = !!connection;
  const startPreset = connection ? presetFor(connection) : (initialPreset ?? 'openai');
  const presetDef = PROVIDER_PRESETS.find((p) => p.id === startPreset)!;
  const [preset, setPreset] = useState<ProviderPresetId>(startPreset);
  const [form, setForm] = useState<ConnectionInput>(() =>
    connection
      ? {
          name: connection.name,
          kind: connection.kind,
          base_url: connection.base_url,
          enabled: connection.enabled,
          models: connection.models,
          supports_websocket: connection.supports_websocket,
        }
      : { name: presetDef.name, kind: presetDef.kind, base_url: presetDef.baseUrl, enabled: true, models: [], supports_websocket: presetDef.websocket },
  );
  const [apiKey, setApiKey] = useState('');
  const [showKey, setShowKey] = useState(false);
  const [errors, setErrors] = useState<ConnectionErrors>({});
  const [serverError, setServerError] = useState<string | null>(null);
  const [submitted, setSubmitted] = useState(false);
  const save = useSaveConnection();
  const toast = useToast();

  const p = PROVIDER_PRESETS.find((x) => x.id === preset)!;
  const isCodex = form.kind === 'codex';
  const wsAllowed = form.kind === 'openai' || form.kind === 'codex';
  const local = useMemo(() => {
    try {
      return isLoopbackHost(new URL(form.base_url).hostname);
    } catch {
      return false;
    }
  }, [form.base_url]);

  const update = (patch: Partial<ConnectionInput>) => {
    setForm((f) => {
      const next = { ...f, ...patch };
      if (submitted) setErrors(validateConnection({ ...next, api_key: apiKey }));
      return next;
    });
  };

  const choosePreset = (id: ProviderPresetId) => {
    const def = PROVIDER_PRESETS.find((x) => x.id === id)!;
    setPreset(id);
    const namedAfterPreset = PROVIDER_PRESETS.some((x) => x.name === form.name) || COMPATIBLE_ENDPOINTS.some((x) => x.label === form.name) || !form.name.trim();
    update({
      kind: def.kind,
      base_url: def.baseUrl,
      supports_websocket: def.websocket,
      name: namedAfterPreset ? def.name : form.name,
      // Keep models the user typed only if they still make sense for the provider family.
      models: def.kind === form.kind ? form.models : [],
    });
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitted(true);
    setServerError(null);
    const input = normalizeInput({ ...form, api_key: apiKey });
    const errs = validateConnection(input);
    setErrors(errs);
    if (Object.keys(errs).length) {
      const firstField = (['name', 'base_url', 'api_key', 'models'] as const).find((k) => errs[k]);
      document.getElementById(`conn-${firstField ?? 'name'}`)?.focus();
      return;
    }
    onBusy(true);
    try {
      const saved = await save.mutateAsync({ id: connection?.id, input });
      setApiKey('');
      onDone();
      // Check the new/changed connection right away so problems surface now, not at first use.
      // (A plain promise: this form unmounts when the sheet closes, which would drop mutate() callbacks.)
      if (saved?.id && saved.enabled) {
        api
          .testConnection(saved.id)
          .then((r) =>
            toast({
              tone: r.ok ? 'ok' : 'err',
              title: r.ok ? `${saved.name} is ready` : `${saved.name} saved, but the check failed`,
              message: r.ok ? `Provider reachable in ${Math.round(r.latency_ms ?? 0)} ms.` : `${r.message}${r.status ? ` (HTTP ${r.status})` : ''}`,
            }),
          )
          .catch(() => toast({ tone: 'ok', title: editing ? 'Connection updated' : 'Connection added', message: saved.name }));
      } else {
        toast({ tone: 'ok', title: editing ? 'Connection updated' : 'Connection added', message: saved?.name });
      }
    } catch (err) {
      setServerError(err instanceof ApiError && err.status === 404 ? 'This connection no longer exists. It may have been deleted elsewhere.' : errorMessage(err));
    } finally {
      onBusy(false);
    }
  };

  const describe = (field: keyof ConnectionErrors, hint?: boolean) => (errors[field] ? `conn-${field}-error` : hint ? `conn-${field}-hint` : undefined);

  return (
    <form className="stack" onSubmit={submit} noValidate>
      {editing && isCodex ? (
        <Callout tone="info" title="Codex subscription account">
          Signed in through ChatGPT and refreshed automatically. To switch accounts, sign in with Codex on the gateway machine and import again.
        </Callout>
      ) : (
        <fieldset className="fieldset">
          <legend className="field-label">Provider</legend>
          <div className="choice-grid two-up" role="radiogroup" aria-label="Provider">
            {PROVIDER_PRESETS.map((x) => (
              <button
                key={x.id}
                type="button"
                role="radio"
                aria-checked={preset === x.id}
                className="choice"
                onClick={() => choosePreset(x.id)}
              >
                <KindMark kind={x.kind} size="sm" />
                <span className="choice-text">
                  <span className="choice-title">{x.label}</span>
                  <span className="choice-desc">{x.description}</span>
                </span>
              </button>
            ))}
          </div>
        </fieldset>
      )}

      {preset === 'compatible' && !isCodex ? (
        <div className="chips" aria-label="Common endpoints">
          {COMPATIBLE_ENDPOINTS.map((ep) => (
            <button
              key={ep.label}
              type="button"
              className="chip-button"
              aria-pressed={form.base_url === ep.baseUrl}
              onClick={() => {
                const namedAfterPreset = COMPATIBLE_ENDPOINTS.some((x) => x.label === form.name) || !form.name.trim();
                update({ base_url: ep.baseUrl, name: namedAfterPreset ? ep.label : form.name });
              }}
            >
              {ep.label}
            </button>
          ))}
        </div>
      ) : null}

      <Field label="Name" htmlFor="conn-name" error={errors.name} hint="Shown in routes and activity. Name accounts so you can tell them apart, e.g. “Codex · work”.">
        <input
          id="conn-name"
          className="input"
          value={form.name}
          onChange={(e) => update({ name: e.target.value })}
          aria-invalid={!!errors.name || undefined}
          aria-describedby={describe('name', true)}
          maxLength={120}
          autoComplete="off"
        />
      </Field>

      <Field
        label="Base URL"
        htmlFor="conn-base_url"
        error={errors.base_url}
        hint={local ? 'Local server: plain http:// is fine here.' : 'Include the version segment, e.g. …/v1.'}
      >
        <input
          id="conn-base_url"
          className="input mono"
          value={form.base_url}
          onChange={(e) => update({ base_url: e.target.value })}
          aria-invalid={!!errors.base_url || undefined}
          aria-describedby={describe('base_url', true)}
          inputMode="url"
          autoComplete="off"
          spellCheck={false}
          disabled={isCodex}
        />
      </Field>

      {isCodex ? null : (
        <Field
          label="API key"
          htmlFor="conn-api_key"
          optional={editing || local}
          error={errors.api_key}
          aside={editing ? connection?.credential_present ? <Badge tone="ok" icon={Lock}>Key stored</Badge> : <Badge tone="warn">No key stored</Badge> : null}
          hint={
            editing
              ? connection?.credential_present
                ? 'Leave blank to keep the stored key. Paste a new one to replace it.'
                : 'Paste a key to authenticate with this provider.'
              : local
                ? 'Most local servers don’t need a key.'
                : 'Stored on the gateway machine only. It is never sent back to the browser.'
          }
        >
          <div className="input-group">
            <input
              id="conn-api_key"
              className="input mono"
              type={showKey ? 'text' : 'password'}
              value={apiKey}
              onChange={(e) => {
                setApiKey(e.target.value);
                if (submitted) setErrors(validateConnection({ ...form, api_key: e.target.value }));
              }}
              placeholder={editing && connection?.credential_present ? '•••••••• (unchanged)' : p.keyHint}
              aria-invalid={!!errors.api_key || undefined}
              aria-describedby={describe('api_key', true)}
              autoComplete="new-password"
              autoCapitalize="off"
              spellCheck={false}
              data-1p-ignore
            />
            <Button variant="ghost" size="sm" iconOnly icon={showKey ? EyeOff : Eye} aria-label={showKey ? 'Hide key' : 'Show key'} aria-pressed={showKey} onClick={() => setShowKey((s) => !s)} />
          </div>
        </Field>
      )}

      <Field
        label="Models"
        htmlFor="conn-models"
        error={errors.models}
        hint="Exact upstream model IDs. Clients can request these directly, or you can map friendlier names in Routes. Paste a comma-separated list to add many."
      >
        <TagInput
          id="conn-models"
          label="Models"
          value={form.models}
          onChange={(models) => update({ models })}
          suggestions={isCodex ? [] : p.suggestions}
          placeholder={p.suggestions[0] ? `e.g. ${p.suggestions[0]}` : 'Type a model ID and press Enter'}
          invalid={!!errors.models}
          describedBy={describe('models', true)}
        />
      </Field>

      <div className="stack-sm">
        <div className="switch-row">
          <div className="stack-sm" style={{ gap: 2 }}>
            <label htmlFor="conn-ws" className="field-label" style={{ justifyContent: 'flex-start' }}>
              Responses WebSocket
            </label>
            <span className="field-hint" id="conn-ws-hint">
              {wsAllowed
                ? 'Let clients like Codex keep one socket open for many responses. Enable only if the upstream supports OpenAI’s WebSocket mode.'
                : 'Only OpenAI and Codex connections support WebSocket mode. SSE streaming still works.'}
            </span>
          </div>
          <Switch
            id="conn-ws"
            checked={form.supports_websocket && wsAllowed}
            onChange={(v) => update({ supports_websocket: v })}
            label="Responses WebSocket"
            describedBy="conn-ws-hint"
            disabled={!wsAllowed}
          />
        </div>
        <div className="switch-row">
          <div className="stack-sm" style={{ gap: 2 }}>
            <label htmlFor="conn-enabled" className="field-label" style={{ justifyContent: 'flex-start' }}>
              Enabled
            </label>
            <span className="field-hint" id="conn-enabled-hint">
              Disabled connections keep their settings but receive no traffic.
            </span>
          </div>
          <Switch id="conn-enabled" checked={form.enabled} onChange={(v) => update({ enabled: v })} label="Enabled" describedBy="conn-enabled-hint" />
        </div>
      </div>

      {serverError ? (
        <Callout tone="err" title="Couldn’t save" role="alert">
          {serverError}
        </Callout>
      ) : null}

      <div className="form-actions">
        <Button onClick={onDone} disabled={save.isPending}>
          Cancel
        </Button>
        <Button type="submit" variant="primary" icon={KeyRound} loading={save.isPending}>
          {editing ? 'Save changes' : 'Add connection'}
        </Button>
      </div>
    </form>
  );
}
