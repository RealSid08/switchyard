import { Check, Copy, KeyRound, Plus, RefreshCw, ShieldCheck, SquareTerminal, Trash2, TriangleAlert } from 'lucide-react';
import { useMemo, useState, type FormEvent } from 'react';
import { useCreateKey, useKeys, useRevokeKey } from '../app/queries';
import { navigate, useLocation } from '../app/router';
import { CodeBlock, copyText } from '../components/Code';
import { Dialog } from '../components/Dialog';
import { useConfirm, useToast } from '../components/feedback';
import { Button, Callout, EmptyState, Field, PageHead, Skeleton } from '../components/ui';
import { ApiError, errorMessage } from '../lib/api';
import { formatDateTime, formatRelative, toMillis } from '../lib/format';
import { KEY_ENV } from '../lib/snippets';
import type { ApiKey, CreatedApiKey } from '../lib/types';

export function KeysPage() {
  const { search } = useLocation();
  const keys = useKeys();
  const createOpen = new URLSearchParams(search).get('new') === '1';
  const sorted = useMemo(() => [...(keys.data ?? [])].sort((a, b) => toMillis(b.created_at) - toMillis(a.created_at)), [keys.data]);
  const close = () => navigate('/keys', { replace: true });

  return (
    <>
      <PageHead
        title="API keys"
        description="Client keys let your tools use the gateway. Give each tool its own key so you can revoke one without breaking the rest."
        actions={
          keys.data?.length ? (
            <Button variant="primary" icon={Plus} onClick={() => navigate('/keys?new=1')}>
              Create key
            </Button>
          ) : null
        }
      />
      {keys.isPending ? (
        <div className="card" role="status" aria-busy aria-label="Loading keys">
          {[0, 1, 2].map((i) => (
            <div key={i} className="row" style={{ padding: '14px 20px', gap: 16 }}>
              <Skeleton w="25%" />
              <Skeleton w="15%" />
              <Skeleton w="20%" />
            </div>
          ))}
        </div>
      ) : keys.isError ? (
        <Callout tone="err" title="Couldn’t load keys" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => keys.refetch()}>Retry</Button>}>
          {errorMessage(keys.error)}
        </Callout>
      ) : !sorted.length ? (
        <div className="card">
          <EmptyState
            icon={KeyRound}
            title="No client keys yet"
            actions={
              <Button variant="primary" icon={Plus} onClick={() => navigate('/keys?new=1')}>
                Create your first key
              </Button>
            }
          >
            Every client request needs a key. Name it after the tool that will use it, like “Codex laptop” or “Claude Code”.
          </EmptyState>
        </div>
      ) : (
        <section className="card" aria-label="Client keys">
          <ul className="key-list">
            {sorted.map((k) => (
              <KeyRow key={k.id} k={k} />
            ))}
          </ul>
          <div className="card-foot">
            <span className="row">
              <ShieldCheck aria-hidden width={14} height={14} /> Keys are stored hashed. Lost one? Revoke it and create another.
            </span>
          </div>
        </section>
      )}
      <CreateKeyDialog open={createOpen} onClose={close} existing={keys.data ?? []} />
    </>
  );
}

function KeyRow({ k }: { k: ApiKey }) {
  const revoke = useRevokeKey();
  const confirm = useConfirm();
  const toast = useToast();
  const onRevoke = async () => {
    const ok = await confirm({
      title: `Revoke “${k.name}”?`,
      message: 'Any tool using this key starts getting 401 errors immediately. This can’t be undone.',
      confirmLabel: 'Revoke key',
      danger: true,
    });
    if (!ok) return;
    revoke.mutate(k.id, {
      onSuccess: () => toast({ tone: 'ok', title: `Revoked “${k.name}”` }),
      onError: (e) => {
        if (e instanceof ApiError && e.status === 404) toast({ tone: 'info', title: 'Already revoked' });
        else toast({ tone: 'err', title: 'Couldn’t revoke key', message: errorMessage(e) });
      },
    });
  };
  return (
    <li className="key-row">
      <KeyRound aria-hidden className="muted" width={16} height={16} />
      <div className="key-main">
        <span className="key-name truncate">{k.name}</span>
        <span className="mono muted small">{k.prefix}…</span>
      </div>
      <span className="muted small key-created" title={formatDateTime(k.created_at)}>
        Created {formatRelative(k.created_at)}
      </span>
      <Button size="sm" variant="danger-ghost" icon={Trash2} onClick={onRevoke} loading={revoke.isPending}>
        Revoke
      </Button>
    </li>
  );
}

function CreateKeyDialog({ open, onClose, existing }: { open: boolean; onClose: () => void; existing: ApiKey[] }) {
  const [created, setCreated] = useState<CreatedApiKey | null>(null);
  const [copied, setCopied] = useState(false);
  const confirm = useConfirm();

  // The plaintext key lives only in this component's state and is dropped on close.
  const finish = async () => {
    if (created && !copied) {
      const ok = await confirm({
        title: 'Close without copying the key?',
        message: 'This is the only time it’s shown. If you close now you’ll need to create a new key.',
        confirmLabel: 'Close anyway',
        danger: true,
      });
      if (!ok) return;
    }
    setCreated(null);
    setCopied(false);
    onClose();
  };

  return (
    <Dialog
      open={open}
      onClose={finish}
      width={520}
      title={created ? 'Copy your new key' : 'Create a client key'}
      description={created ? 'This is the only time Switchyard will show it.' : 'Keys authenticate tools to the gateway. They never grant admin access.'}
    >
      {created ? (
        <Reveal created={created} copied={copied} onCopied={() => setCopied(true)} onDone={finish} />
      ) : (
        <CreateForm existing={existing} onCreated={setCreated} onCancel={finish} />
      )}
    </Dialog>
  );
}

function CreateForm({ existing, onCreated, onCancel }: { existing: ApiKey[]; onCreated: (k: CreatedApiKey) => void; onCancel: () => void }) {
  const [name, setName] = useState('');
  const [error, setError] = useState<string | null>(null);
  const create = useCreateKey();
  const submit = (e: FormEvent) => {
    e.preventDefault();
    const n = name.trim();
    if (!n) return setError('Name the key after the tool that will use it.');
    if (n.length > 100) return setError('Keep the name under 100 characters.');
    setError(null);
    create.mutate(n, {
      onSuccess: (k) => onCreated(k),
      onError: (err) => setError(errorMessage(err)),
    });
  };
  const duplicate = existing.some((k) => k.name.toLowerCase() === name.trim().toLowerCase());
  return (
    <form className="stack" onSubmit={submit} noValidate>
      <Field label="Name" htmlFor="key-name" error={error ?? undefined} hint={duplicate ? 'You already have a key with this name. That’s allowed, but harder to tell apart later.' : 'For example “Codex on laptop” or “CI”.'}>
        <input
          id="key-name"
          className="input"
          value={name}
          onChange={(e) => setName(e.target.value)}
          maxLength={120}
          autoComplete="off"
          aria-invalid={!!error || undefined}
          aria-describedby={error ? 'key-name-error' : 'key-name-hint'}
        />
      </Field>
      <div className="form-actions">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="primary" icon={KeyRound} loading={create.isPending}>
          Create key
        </Button>
      </div>
    </form>
  );
}

function Reveal({ created, copied, onCopied, onDone }: { created: CreatedApiKey; copied: boolean; onCopied: () => void; onDone: () => void }) {
  const [failed, setFailed] = useState(false);
  const copy = async () => {
    const ok = await copyText(created.key);
    if (ok) onCopied();
    setFailed(!ok);
  };
  return (
    <div className="stack">
      <div className="key-reveal">
        <span className="muted xs">{created.name}</span>
        <code className="key-secret" aria-label="New client key">
          {created.key}
        </code>
        <Button variant={copied ? 'default' : 'primary'} icon={copied ? Check : Copy} onClick={copy} data-autofocus>
          {copied ? 'Copied' : 'Copy key'}
        </Button>
        <span className="sr-only" role="status" aria-live="polite">
          {copied ? 'Key copied to clipboard' : ''}
        </span>
      </div>
      {failed ? (
        <Callout tone="warn" icon={TriangleAlert}>
          Your browser blocked clipboard access. Select the key above and copy it manually.
        </Callout>
      ) : null}
      <CodeBlock title="Use it in your shell" language="bash" code={`export ${KEY_ENV}=${created.key}`} />
      <Callout tone="warn">
        Store it somewhere safe, like your password manager. Switchyard keeps only a hash and can’t show it again.
      </Callout>
      <div className="form-actions">
        <Button icon={SquareTerminal} onClick={() => navigate('/clients')} disabled={!copied}>
          Set up a client
        </Button>
        <Button variant="primary" onClick={onDone}>
          Done
        </Button>
      </div>
    </div>
  );
}
