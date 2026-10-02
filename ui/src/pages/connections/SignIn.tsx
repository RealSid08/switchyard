import { useQueryClient } from '@tanstack/react-query';
import { CircleCheck, ExternalLink, FlaskConical, LoaderCircle, MonitorSmartphone, RefreshCw, ShieldCheck, TriangleAlert, WifiOff } from 'lucide-react';
import { useEffect, useState, useSyncExternalStore, type FormEvent } from 'react';
import { qk } from '../../app/queries';
import { navigate } from '../../app/router';
import { CopyButton } from '../../components/Code';
import { Dialog } from '../../components/Dialog';
import { useToast } from '../../components/feedback';
import { Badge, Button, Callout, Field, KindMark } from '../../components/ui';
import { api } from '../../lib/client';
import { isLoopbackHost } from '../../lib/connections';
import { CALLBACK_PORTS, OAuthFlowController, PROVIDER_COPY, formatCountdown } from '../../lib/oauth';
import type { Connection, OAuthProvider } from '../../lib/types';

/**
 * Independent browser sign-in. The gateway owns the resulting login and refreshes
 * it; nothing on disk from Codex or Claude Code is used or changed.
 */
export function SignInDialog({ provider, onClose }: { provider: OAuthProvider | null; onClose: () => void }) {
  const copy = provider ? PROVIDER_COPY[provider] : null;
  return (
    <Dialog
      open={!!provider}
      onClose={onClose}
      width={560}
      icon={provider ? <KindMark kind={provider === 'codex' ? 'codex' : 'anthropic'} size="lg" /> : null}
      title={copy ? `Sign in with ${copy.name}` : ''}
      description={copy ? `Add a ${copy.account} that Switchyard keeps signed in on its own.` : undefined}
    >
      {provider ? <SignInFlow key={provider} provider={provider} onClose={onClose} /> : null}
    </Dialog>
  );
}

function SignInFlow({ provider, onClose }: { provider: OAuthProvider; onClose: () => void }) {
  const qc = useQueryClient();
  const toast = useToast();
  const [known] = useState(() => new Set((qc.getQueryData<Connection[]>(qk.connections) ?? []).map((c) => c.id)));
  const [ctl] = useState(
    () =>
      new OAuthFlowController(api, provider, {
        onComplete: (conn) => {
          for (const key of [qk.connections, qk.models, qk.overview, qk.routes]) void qc.invalidateQueries({ queryKey: key });
          toast({
            tone: 'ok',
            title: conn ? `Signed in: ${conn.name}` : 'Signed in',
            message: conn && known.has(conn.id) ? 'Refreshed the existing account.' : 'Switchyard will keep this account signed in.',
          });
        },
      }),
  );
  const s = useSyncExternalStore(ctl.subscribe, () => ctl.snapshot);
  const [input, setInput] = useState('');
  const remote = !isLoopbackHost(window.location.hostname);
  const c = PROVIDER_COPY[provider];

  useEffect(() => {
    void ctl.start();
    // Leaving the page mid-flow still releases the gateway's callback port.
    const onHide = () => {
      const id = ctl.snapshot.id;
      if (id && (ctl.snapshot.phase === 'pending' || ctl.snapshot.phase === 'submitting')) {
        void fetch(`/api/oauth/${encodeURIComponent(id)}`, { method: 'DELETE', credentials: 'include', keepalive: true }).catch(() => {});
      }
    };
    window.addEventListener('pagehide', onHide);
    return () => {
      window.removeEventListener('pagehide', onHide);
      ctl.dispose();
    };
  }, [ctl]);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    void ctl.submitCallback(input);
  };

  if (s.phase === 'idle' || s.phase === 'starting') {
    return (
      <div className="signin-wait" role="status">
        <LoaderCircle className="spin" aria-hidden />
        <span>Preparing a secure sign-in link…</span>
      </div>
    );
  }

  if (s.phase === 'complete') {
    const conn = s.connection;
    return (
      <div className="stack">
        <div className="signin-done" role="status">
          <CircleCheck aria-hidden />
          <div className="stack-sm" style={{ gap: 2 }}>
            <strong>{conn ? conn.name : 'Signed in'}</strong>
            <span className="muted small">{conn && known.has(conn.id) ? 'Existing account refreshed.' : 'New account connected.'}</span>
          </div>
        </div>
        {conn?.models.length ? (
          <div className="chips" aria-label="Models">
            {conn.models.map((m) => (
              <span className="model-chip" key={m}>
                {m}
              </span>
            ))}
          </div>
        ) : null}
        <p className="trust-note">
          <ShieldCheck aria-hidden />
          Switchyard refreshes this sign-in itself. It doesn’t depend on, or change, your {c.cli} login.
        </p>
        <div className="form-actions">
          {conn?.models[0] ? (
            <Button icon={FlaskConical} onClick={() => navigate(`/playground?model=${encodeURIComponent(conn.models[0])}`)}>
              Try it
            </Button>
          ) : null}
          <Button variant="primary" onClick={onClose} data-autofocus autoFocus>
            Done
          </Button>
        </div>
      </div>
    );
  }

  if (s.phase === 'error' || s.phase === 'expired' || s.phase === 'cancelled') {
    const title = s.phase === 'expired' ? 'Sign-in expired' : s.errorKind === 'busy' ? 'Sign-in port is busy' : s.phase === 'cancelled' ? 'Sign-in cancelled' : 'Sign-in didn’t finish';
    return (
      <div className="stack">
        <Callout tone={s.phase === 'expired' ? 'warn' : 'err'} title={title} role="alert">
          {s.message}
          {s.errorKind === 'busy' ? (
            <div className="muted small" style={{ marginTop: 4 }}>
              The port is on the gateway machine. Once it’s free, try again.
            </div>
          ) : null}
        </Callout>
        <div className="form-actions">
          <Button onClick={onClose}>Close</Button>
          <Button variant="primary" icon={RefreshCw} onClick={() => void ctl.start()} data-autofocus autoFocus>
            Try again
          </Button>
        </div>
      </div>
    );
  }

  // pending / submitting
  const url = s.authorizationUrl ?? '';
  return (
    <div className="stack">
      <ol className="signin-steps">
        <li>
          <span className="signin-step-n" aria-hidden>
            1
          </span>
          <div className="stack-sm">
            <strong>Approve access in your browser</strong>
            <span className="muted small">Sign in with the {c.name} account you want Switchyard to use. Add more accounts by repeating this.</span>
            <div className="row row-wrap">
              <a className="btn btn-primary" href={url} target="_blank" rel="noopener noreferrer" data-autofocus>
                <ExternalLink aria-hidden />
                Open sign-in page
              </a>
              <CopyButton text={url} label="Copy sign-in link" showLabel variant="default" />
            </div>
          </div>
        </li>
        <li>
          <span className="signin-step-n" aria-hidden>
            2
          </span>
          <div className="stack-sm">
            <strong>{remote ? 'Paste the address you land on' : 'Come back here'}</strong>
            <span className="muted small">
              {remote
                ? `This browser isn’t on the gateway machine, so the final redirect to localhost:${CALLBACK_PORTS[provider]} can’t reach Switchyard. After approving, the page will fail to load. That’s expected: copy its full address and paste it below.`
                : 'This finishes on its own once you approve. Nothing to copy.'}
            </span>
          </div>
        </li>
      </ol>

      <div className="signin-status" role="status" aria-live="polite">
        {s.reconnecting ? <WifiOff aria-hidden /> : <LoaderCircle className="spin" aria-hidden />}
        <span>{s.phase === 'submitting' ? 'Finishing sign-in…' : s.reconnecting ? 'Lost touch with the gateway. Still waiting…' : 'Waiting for you to approve…'}</span>
        <span className="spacer" />
        {s.remaining !== null ? (
          <Badge tone={s.remaining < 60 ? 'warn' : undefined}>
            <span className="num">expires in {formatCountdown(s.remaining)}</span>
          </Badge>
        ) : null}
      </div>

      <details className="signin-paste" open={remote || undefined}>
        <summary>
          <MonitorSmartphone aria-hidden />
          {remote ? 'Paste the callback address' : 'Signing in from a different computer, or it didn’t return?'}
        </summary>
        <form className="stack-sm" onSubmit={submit} noValidate>
          <Field
            label="Callback address"
            htmlFor="oauth-callback"
            error={s.callbackError ?? undefined}
            hint={<>From the browser’s address bar after approving, for example <code>{c.example}</code></>}
          >
            <div className="row">
              <input
                id="oauth-callback"
                className="input mono"
                value={input}
                onChange={(e) => setInput(e.target.value)}
                placeholder={`http://localhost:${CALLBACK_PORTS[provider]}/…`}
                autoComplete="off"
                autoCapitalize="off"
                spellCheck={false}
                aria-invalid={!!s.callbackError || undefined}
                aria-describedby={s.callbackError ? 'oauth-callback-error' : 'oauth-callback-hint'}
              />
              <Button type="submit" loading={s.phase === 'submitting'} disabled={!input.trim()}>
                Finish
              </Button>
            </div>
          </Field>
        </form>
      </details>

      <p className="trust-note">
        <ShieldCheck aria-hidden />
        Switchyard never sees your password. It receives a sign-in token from {c.name} and stores it on the gateway machine. Your {c.cli} files aren’t used or
        changed.
      </p>
      {s.remaining !== null && s.remaining < 30 ? (
        <Callout tone="warn" icon={TriangleAlert}>
          This link is about to expire. If it does, start again; it only takes a moment.
        </Callout>
      ) : null}
      <div className="form-actions">
        <Button onClick={onClose}>Cancel sign-in</Button>
      </div>
    </div>
  );
}
