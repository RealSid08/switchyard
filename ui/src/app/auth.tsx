import { useQueryClient } from '@tanstack/react-query';
import { Eye, EyeOff, KeyRound, LogIn, RefreshCw, ServerOff, ShieldAlert } from 'lucide-react';
import { createContext, useCallback, useContext, useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { CodeBlock } from '../components/Code';
import { BrandMark } from '../components/BrandMark';
import { Button, Callout, Field } from '../components/ui';
import { bootstrapSession, cleanToken, signInWithToken, tokenStore, type AuthState } from '../lib/auth';
import { api, onUnauthorized } from '../lib/client';

interface AuthContextValue {
  mode: 'cookie' | 'token';
  signOut: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue>({ mode: 'cookie', signOut: async () => {} });

export function useAuth() {
  return useContext(AuthContext);
}

export function AuthProvider({ children }: { children: (ready: boolean) => ReactNode }) {
  const qc = useQueryClient();
  const [state, setState] = useState<AuthState | { status: 'signed-out'; mode: 'cookie' | 'token' }>({ status: 'checking' });
  const inflight = useRef<Promise<AuthState> | null>(null);
  const signedOut = useRef(false);

  const run = useCallback(() => {
    inflight.current ??= bootstrapSession(api, tokenStore).finally(() => {
      inflight.current = null;
    });
    return inflight.current;
  }, []);

  useEffect(() => {
    let alive = true;
    void run().then((s) => alive && setState(s));
    return () => {
      alive = false;
    };
  }, [run]);

  // A 401 mid-session usually means the gateway restarted (new session secret) or
  // the cookie aged out. Re-establish silently; only show the gate if that fails.
  useEffect(
    () =>
      onUnauthorized(() => {
        if (signedOut.current) return;
        void run().then((s) => {
          setState(s);
          if (s.status === 'ready') void qc.invalidateQueries();
        });
      }),
    [run, qc],
  );

  // Unreachable: keep retrying quietly so the dashboard comes back on its own.
  useEffect(() => {
    if (state.status !== 'error' || state.kind !== 'unreachable') return;
    const t = window.setTimeout(() => void run().then(setState), 4000);
    return () => window.clearTimeout(t);
  }, [state, run]);

  // Sign out ends this browser's server session, forgets any pasted token and drops
  // every cached response. On the gateway machine a fresh session is one click away
  // (the screen says so rather than silently signing back in).
  const signOut = useCallback(async () => {
    const mode = state.status === 'ready' ? state.mode : 'cookie';
    signedOut.current = true;
    await api.logout().catch(() => {});
    tokenStore.clear();
    qc.clear();
    setState({ status: 'signed-out', mode });
  }, [qc, state]);

  const signInAgain = useCallback(() => {
    signedOut.current = false;
    setState({ status: 'checking' });
    void run().then(setState);
  }, [run]);

  if (state.status === 'checking') return <Boot />;
  if (state.status === 'signed-out') return <SignedOut local={state.mode === 'cookie'} onSignIn={signInAgain} />;
  if (state.status === 'needs-token') return <TokenGate reason={state.reason} onReady={setState} />;
  if (state.status === 'error') return <ErrorGate state={state} onRetry={() => run().then(setState)} />;
  return <AuthContext.Provider value={{ mode: state.mode, signOut }}>{children(true)}</AuthContext.Provider>;
}

/** `local`: the session came from the loopback cookie bootstrap, not a pasted token. */
function SignedOut({ local, onSignIn }: { local: boolean; onSignIn: () => void }) {
  return (
    <main className="gate">
      <div className="gate-card">
        <GateBrand />
        <div className="stack-sm">
          <h1>You’re signed out</h1>
          <p>
            {local
              ? 'Your admin session ended and cached data was cleared. Because this browser is on the gateway machine, signing back in takes one click, and reloading the page will also sign you in automatically.'
              : 'Your admin session ended, cached data was cleared and the admin token was removed from this tab.'}
          </p>
        </div>
        <Button variant="primary" icon={LogIn} onClick={onSignIn} block data-autofocus autoFocus>
          {local ? 'Sign in again' : 'Sign in with a token'}
        </Button>
        <p className="small muted">The gateway keeps running and serving your clients while you’re signed out.</p>
      </div>
    </main>
  );
}

function Boot() {
  return (
    <div className="boot" role="status" aria-live="polite">
      <div style={{ display: 'grid', justifyItems: 'center', gap: 12 }}>
        <BrandMark />
        <span className="small">Connecting to your gateway…</span>
      </div>
    </div>
  );
}

function GateBrand() {
  return (
    <div className="brand">
      <BrandMark />
      <span className="brand-name">Switchyard</span>
    </div>
  );
}

function TokenGate({ reason, onReady }: { reason: 'remote' | 'expired' | 'rejected'; onReady: (s: AuthState) => void }) {
  const [value, setValue] = useState('');
  const [show, setShow] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(reason === 'expired' ? 'Your saved token no longer works. The gateway may have a new admin token.' : null);

  const cleaned = cleanToken(value);
  const looksLikeClientKey = cleaned.startsWith('sy_') && !cleaned.startsWith('sy_admin_');

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!cleaned) {
      setError('Paste the admin token to continue.');
      return;
    }
    setBusy(true);
    setError(null);
    const next = await signInWithToken(api, tokenStore, value);
    setBusy(false);
    if (next.status === 'ready') {
      setValue('');
      onReady(next);
    } else if (next.status === 'needs-token') {
      setError(looksLikeClientKey ? 'That is a client key. The dashboard needs the admin token.' : 'The gateway rejected that token. Check you copied all of it.');
    } else if (next.status === 'error') {
      setError(next.message);
    }
  };

  return (
    <main className="gate">
      <form className="gate-card" onSubmit={submit} noValidate>
        <GateBrand />
        <div className="stack-sm">
          <h1>Sign in to the control room</h1>
          <p>
            You're opening Switchyard from another machine, so the browser can't get a local session automatically. Paste the admin token from
            the machine running the gateway.
          </p>
        </div>
        <CodeBlock code={'cat "$(switchyard token-path)"'} language="bash" compact />
        <Field label="Admin token" htmlFor="admin-token" error={error ?? undefined} hint={looksLikeClientKey ? 'This looks like a client key (sy_…). Admin tokens start with sy_admin_.' : 'Kept in this tab only (sessionStorage). Closing the tab forgets it.'}>
          <div className="input-group">
            <input
              id="admin-token"
              className="input mono"
              type={show ? 'text' : 'password'}
              value={value}
              onChange={(e) => setValue(e.target.value)}
              autoComplete="off"
              autoCapitalize="off"
              spellCheck={false}
              aria-invalid={!!error || undefined}
              aria-describedby={error ? 'admin-token-error' : 'admin-token-hint'}
              placeholder="sy_admin_…"
              autoFocus
            />
            <Button variant="ghost" size="sm" iconOnly icon={show ? EyeOff : Eye} aria-label={show ? 'Hide token' : 'Show token'} aria-pressed={show} onClick={() => setShow((s) => !s)} />
          </div>
        </Field>
        <Button type="submit" variant="primary" icon={KeyRound} loading={busy} block>
          Sign in
        </Button>
        <Callout tone="info" quiet>
          Use HTTPS (or a private network like Tailscale) when administering Switchyard remotely. Never give clients the admin token; create a client key
          instead.
        </Callout>
      </form>
    </main>
  );
}

function ErrorGate({ state, onRetry }: { state: Extract<AuthState, { status: 'error' }>; onRetry: () => Promise<unknown> }) {
  const [busy, setBusy] = useState(false);
  const retry = async () => {
    setBusy(true);
    await onRetry();
    setBusy(false);
  };
  const Icon = state.kind === 'forbidden' ? ShieldAlert : ServerOff;
  return (
    <main className="gate">
      <div className="gate-card">
        <GateBrand />
        <div className="stack-sm">
          <h1 className="row">
            <Icon width={20} height={20} aria-hidden style={{ color: 'var(--err-text)' }} />
            {state.kind === 'unreachable' ? 'Gateway unreachable' : state.kind === 'forbidden' ? 'Access blocked' : 'Something went wrong'}
          </h1>
          <p role="alert">{state.message}</p>
        </div>
        {state.kind === 'unreachable' ? (
          <>
            <p className="small muted">Start it with the command below. This page reconnects automatically as soon as the gateway is up.</p>
            <CodeBlock code="switchyard" language="bash" compact />
          </>
        ) : state.kind === 'forbidden' ? (
          <p className="small muted">
            Switchyard only accepts administration from its own origin. Open the dashboard at the gateway's address directly rather than through another site
            or a proxy that rewrites the Origin header.
          </p>
        ) : null}
        <Button variant="primary" icon={RefreshCw} onClick={retry} loading={busy} block>
          Retry now
        </Button>
      </div>
    </main>
  );
}
