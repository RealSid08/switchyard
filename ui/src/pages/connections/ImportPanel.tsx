import { useQueryClient } from '@tanstack/react-query';
import { Download, FolderOpen, LogIn, Plus, ShieldCheck } from 'lucide-react';
import { useState, type FormEvent } from 'react';
import { qk, useImport } from '../../app/queries';
import { useToast } from '../../components/feedback';
import { Button, Callout, Field, KindMark, kindLabel } from '../../components/ui';
import { navigate } from '../../app/router';
import { SignInDialog } from './SignIn';
import { errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import type { Connection, ImportResult, ImportSource, OAuthProvider } from '../../lib/types';

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
  antigravity: 'Sign in with the Antigravity CLI on the gateway machine, then import again. Or use Sign in.',
  opencode: 'Add a Zen or Go key in OpenCode on the gateway machine (opencode auth), then import again.',
  opencode_go: 'Add a Go key in OpenCode on the gateway machine (opencode auth), then import again.',
};

const SOURCE_LABEL: Record<ImportSource, string> = {
  codex: 'Codex',
  claude: 'Claude Code',
  cliproxy: 'CLIProxyAPI',
  antigravity: 'Antigravity',
  opencode: 'OpenCode',
  opencode_go: 'OpenCode Go',
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

/**
 * Every way to bring an account, in one place: browser sign-in or CLI import for
 * subscriptions, API keys for providers, and CLIProxyAPI migration.
 */
export function ConnectOptions({ apiProviders = true }: { apiProviders?: boolean }) {
  const { run, outcome, pending } = useImportAction();
  const toast = useToast();
  const [path, setPath] = useState('');
  const [pathError, setPathError] = useState<string | null>(null);
  const [signIn, setSignIn] = useState<OAuthProvider | null>(null);

  const go = async (source: ImportSource, p?: string) => {
    const o = await run(source, p);
    if (o.error) return;
    toast({
      tone: 'ok',
      title: `${SOURCE_LABEL[source]} import complete`,
      message: `${describeImport(o)} Original credential files were not modified.`,
    });
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

  const accounts: { provider: OAuthProvider | null; source: ImportSource; kind: string; title: string; sub: string; cli: string; importLabel?: string }[] = [
    { provider: 'codex', source: 'codex', kind: 'codex', title: 'ChatGPT', sub: 'Codex subscription', cli: 'Codex CLI' },
    { provider: 'claude', source: 'claude', kind: 'anthropic', title: 'Claude', sub: 'Claude Pro or Max subscription', cli: 'Claude Code' },
    { provider: 'antigravity', source: 'antigravity', kind: 'antigravity', title: 'Antigravity', sub: 'Google account with Antigravity', cli: 'Antigravity CLI' },
    // OpenCode uses API keys, so there's no browser sign-in: import the keys OpenCode saved.
    { provider: null, source: 'opencode', kind: 'opencode', title: 'OpenCode Zen and Go', sub: 'Keys saved by OpenCode on this machine', cli: 'OpenCode', importLabel: 'Import OpenCode keys' },
  ];

  return (
    <div className="stack">
      <ul className="account-rows" aria-label="Subscription accounts">
        {accounts.map((a) => (
          <li key={a.source} className="account-row">
            <KindMark kind={a.kind} size="lg" />
            <div className="account-row-text">
              <span className="account-row-title">{a.title}</span>
              <span className="muted small">{a.sub}</span>
            </div>
            <div className="account-row-actions">
              {a.provider ? (
                <Button
                  variant="primary"
                  icon={LogIn}
                  onClick={() => setSignIn(a.provider)}
                  disabled={!!pending}
                  aria-label={`Sign in with ${a.title} in the browser`}
                  data-autofocus={a.provider === 'codex' ? true : undefined}
                >
                  Sign in
                </Button>
              ) : null}
              <Button
                icon={Download}
                onClick={() => go(a.source)}
                loading={pending === a.source}
                disabled={!!pending && pending !== a.source}
                aria-label={a.importLabel ?? `Import ${a.cli} login`}
              >
                {a.importLabel ?? `Import ${a.cli}`}
              </Button>
            </div>
          </li>
        ))}
      </ul>
      <dl className="method-legend">
        <div>
          <dt>
            <LogIn aria-hidden /> Sign in
          </dt>
          <dd>A fresh browser sign-in that Switchyard owns and keeps refreshed on its own. Works even from another computer.</dd>
        </div>
        <div>
          <dt>
            <Download aria-hidden /> Import
          </dt>
          <dd>Reuses the login your CLI already saved on the gateway machine, read-only. It follows that CLI: sign in there again if it expires.</dd>
        </div>
      </dl>
      {outcome ? <ImportResultView outcome={outcome} /> : null}

      {apiProviders ? (
        <div className="stack-sm">
          <h3>Or add an API key</h3>
          <div className="provider-buttons">
            {(['openai', 'anthropic', 'gemini', 'compatible'] as const).map((id) => (
              <button key={id} type="button" className="provider-button" onClick={() => navigate(`/connections?new=1&preset=${id}`)}>
                <KindMark kind={id === 'compatible' ? 'openai' : id} size="sm" />
                {id === 'compatible' ? 'OpenAI-compatible' : kindLabel(id)}
                <Plus aria-hidden className="muted" />
              </button>
            ))}
          </div>
        </div>
      ) : null}

      <details className="cliproxy">
        <summary>Migrating from CLIProxyAPI?</summary>
        <form className="stack-sm" onSubmit={submitCliproxy}>
          <Field
            label="Auth file or directory"
            htmlFor="cliproxy-path"
            error={pathError ?? undefined}
            hint="A path on the gateway machine. Imports up to 100 accounts; each follows its CLIProxyAPI file."
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
      </details>
      <p className="trust-note">
        <ShieldCheck aria-hidden />
        Credentials stay on the machine running Switchyard. Imports never modify your Codex, Claude or CLIProxyAPI files, and connecting the same account
        again refreshes it instead of creating a duplicate.
      </p>
      <SignInDialog provider={signIn} onClose={() => setSignIn(null)} />
    </div>
  );
}
