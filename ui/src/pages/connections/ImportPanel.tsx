import { useQueryClient } from '@tanstack/react-query';
import { CircleCheck, FolderOpen, ShieldCheck } from 'lucide-react';
import { useState, type FormEvent } from 'react';
import { qk, useImport } from '../../app/queries';
import { useToast } from '../../components/feedback';
import { Button, Callout, Field, KindMark } from '../../components/ui';
import { errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import type { Connection, ImportResult, ImportSource } from '../../lib/types';

export interface ImportOutcome {
  source: ImportSource;
  result?: ImportResult;
  added: number;
  updated: number;
  /** False when we couldn't tell new accounts from refreshed ones. */
  classified: boolean;
  error?: string;
}

const HINTS: Record<ImportSource, string> = {
  codex: 'Sign in on the gateway machine with `codex login`, then import again.',
  claude: 'Sign in on the gateway machine with `claude` (then /login), then import again.',
  cliproxy: 'Point at a CLIProxyAPI auth .json file or its auth directory on the gateway machine.',
};

/** Shared import action: classifies results as added vs refreshed for multi-account clarity. */
export function useImportAction() {
  const qc = useQueryClient();
  const mutation = useImport();
  const [outcome, setOutcome] = useState<ImportOutcome | null>(null);
  const [pending, setPending] = useState<ImportSource | null>(null);
  const run = async (source: ImportSource, path?: string): Promise<ImportOutcome> => {
    setPending(source);
    // Know what existed before, even if the list hasn't loaded yet, so a re-import
    // reads as "refreshed" rather than "added".
    const known = await qc.ensureQueryData({ queryKey: qk.connections, queryFn: api.connections }).catch(() => null);
    const before = known ? new Set(known.map((c: Connection) => c.id)) : null;
    try {
      const result = await mutation.mutateAsync({ source, path });
      const ids = (result.connections ?? []).map((c) => c.id);
      const added = before ? ids.filter((id) => !before.has(id)).length : ids.length;
      const o: ImportOutcome = { source, result, added, updated: ids.length - added, classified: !!before };
      setOutcome(o);
      return o;
    } catch (e) {
      const o: ImportOutcome = { source, added: 0, updated: 0, classified: true, error: errorMessage(e) };
      setOutcome(o);
      return o;
    } finally {
      setPending(null);
    }
  };
  return { run, outcome, pending, reset: () => setOutcome(null) };
}

export function describeImport(o: ImportOutcome): string {
  if (o.error) return o.error;
  if (!o.classified) {
    const n = o.result?.imported ?? o.added;
    return `Imported ${n} ${n === 1 ? 'account' : 'accounts'}.`;
  }
  const parts: string[] = [];
  if (o.added) parts.push(`added ${o.added} new ${o.added === 1 ? 'account' : 'accounts'}`);
  if (o.updated) parts.push(`refreshed ${o.updated} existing ${o.updated === 1 ? 'account' : 'accounts'}`);
  const summary = parts.length ? parts.join(' and ') : 'nothing new to import';
  return summary.charAt(0).toUpperCase() + summary.slice(1) + '.';
}

export function ImportResultView({ outcome }: { outcome: ImportOutcome }) {
  if (outcome.error) {
    return (
      <Callout tone="err" title="Import failed" role="alert">
        {outcome.error} <span className="muted">{HINTS[outcome.source]}</span>
      </Callout>
    );
  }
  const conns = outcome.result?.connections ?? [];
  return (
    <Callout tone="ok" title={describeImport(outcome)} role="status">
      {conns.length ? (
        <ul className="import-list">
          {conns.map((c) => (
            <li key={c.id}>
              <KindMark kind={c.kind} size="sm" />
              <span className="truncate">{c.name}</span>
              <span className="muted xs">{c.models.length} models</span>
            </li>
          ))}
        </ul>
      ) : null}
      <span className="muted">Your original credential files were not modified.</span>
    </Callout>
  );
}

export function ImportPanel({ onImported }: { onImported?: (o: ImportOutcome) => void }) {
  const { run, outcome, pending } = useImportAction();
  const toast = useToast();
  const [path, setPath] = useState('');
  const [pathError, setPathError] = useState<string | null>(null);

  const go = async (source: ImportSource, p?: string) => {
    const o = await run(source, p);
    if (o.error) return;
    toast({
      tone: 'ok',
      title: `${source === 'codex' ? 'Codex' : source === 'claude' ? 'Claude Code' : 'CLIProxyAPI'} import complete`,
      message: `${describeImport(o)} Original credential files were not modified.`,
    });
    onImported?.(o);
  };

  const submitCliproxy = (e: FormEvent) => {
    e.preventDefault();
    if (!path.trim()) {
      setPathError('Enter the path to a CLIProxyAPI auth file or directory.');
      return;
    }
    setPathError(null);
    void go('cliproxy', path.trim());
  };

  return (
    <div className="stack">
      <div className="import-grid">
        <ImportTile
          kind="codex"
          title="Codex"
          detail="ChatGPT sign-in from ~/.codex/auth.json"
          busy={pending === 'codex'}
          disabled={!!pending}
          onClick={() => go('codex')}
        />
        <ImportTile
          kind="anthropic"
          title="Claude Code"
          detail="~/.claude/.credentials.json or the macOS Keychain"
          busy={pending === 'claude'}
          disabled={!!pending}
          onClick={() => go('claude')}
        />
      </div>
      <form className="stack-sm" onSubmit={submitCliproxy}>
        <Field
          label="Migrating from CLIProxyAPI?"
          htmlFor="cliproxy-path"
          error={pathError ?? undefined}
          hint="Path on the gateway machine to an auth .json file or the auth directory (up to 100 accounts)."
        >
          <div className="row">
            <input
              id="cliproxy-path"
              className="input mono"
              placeholder="~/.cli-proxy-api"
              value={path}
              onChange={(e) => setPath(e.target.value)}
              aria-invalid={!!pathError || undefined}
              autoComplete="off"
              spellCheck={false}
            />
            <Button type="submit" icon={FolderOpen} loading={pending === 'cliproxy'} disabled={!!pending}>
              Import
            </Button>
          </div>
        </Field>
      </form>
      {outcome ? <ImportResultView outcome={outcome} /> : null}
      <p className="trust-note">
        <ShieldCheck aria-hidden />
        Imports read credentials on the machine running Switchyard and copy them into its private store. Your Codex, Claude and CLIProxyAPI files are never
        modified, and importing the same account again refreshes it instead of creating a duplicate.
      </p>
    </div>
  );
}

export function ImportTile({ kind, title, detail, busy, disabled, onClick, done }: { kind: string; title: string; detail: string; busy?: boolean; disabled?: boolean; onClick: () => void; done?: boolean }) {
  return (
    <button type="button" className="import-tile" onClick={onClick} disabled={disabled} aria-busy={busy || undefined}>
      <KindMark kind={kind} size="lg" />
      <span className="import-tile-text">
        <span className="import-tile-title">Import {title}</span>
        <span className="import-tile-detail">{detail}</span>
      </span>
      <span className="import-tile-cta">{busy ? 'Importing…' : done ? <CircleCheck aria-label="Imported" width={16} height={16} /> : 'One click'}</span>
    </button>
  );
}
