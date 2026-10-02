import { ExternalLink, LogOut, Monitor, Moon, Pause, Play, Sun } from 'lucide-react';
import { useAuth } from '../app/auth';
import { useConfig, useOverview } from '../app/queries';
import { usePauseToggle } from '../app/Shell';
import { useTheme, type ThemePref } from '../app/theme';
import { Badge, Button, PageHead, Segmented, Skeleton } from '../components/ui';
import { errorMessage } from '../lib/api';
import { formatDuration, formatNumber } from '../lib/format';

export function SettingsPage() {
  const { pref, setPref } = useTheme();
  const config = useConfig();
  const overview = useOverview();
  const pause = usePauseToggle();
  const auth = useAuth();
  const o = overview.data;
  const c = config.data;
  return (
    <>
      <PageHead title="Settings" description="Gateway controls and preferences for this browser." />
      <div className="settings-grid">
        <section className="card" aria-labelledby="gw-title">
          <div className="card-head">
            <h2 id="gw-title">Gateway</h2>
            {o ? <Badge tone={o.paused ? 'warn' : 'ok'}>{o.paused ? 'Paused' : 'Running'}</Badge> : null}
          </div>
          <div className="card-body stack">
            <div className="switch-row">
              <div className="stack-sm" style={{ gap: 2 }}>
                <strong>{o?.paused ? 'Traffic is paused' : 'Accepting traffic'}</strong>
                <span className="muted small">Pausing refuses new client requests with 503. In-flight requests finish. Survives restarts.</span>
              </div>
              {o ? (
                <Button icon={o.paused ? Play : Pause} variant={o.paused ? 'primary' : 'default'} loading={pause.pending} onClick={() => pause.toggle(!o.paused)}>
                  {o.paused ? 'Resume' : 'Pause'}
                </Button>
              ) : null}
            </div>
            <hr className="divider" />
            {config.isPending ? (
              <Skeleton h={90} />
            ) : config.isError ? (
              <p className="muted small">Couldn’t load gateway configuration: {errorMessage(config.error)}</p>
            ) : c ? (
              <dl className="kv">
                <dt>Listening on</dt>
                <dd className="mono">
                  {c.host}:{c.port}
                </dd>
                <dt>Max in-flight</dt>
                <dd className="num">{formatNumber(c.max_in_flight)} requests</dd>
                <dt>Request timeout</dt>
                <dd className="num">{formatDuration(c.request_timeout_seconds)}</dd>
                <dt>Client keys</dt>
                <dd>{c.requires_api_key ? 'Required on every request' : 'Not required'}</dd>
              </dl>
            ) : null}
            <p className="muted xs">Host, port and limits are set with command-line flags or SWITCHYARD_* environment variables when starting the gateway.</p>
          </div>
        </section>

        <section className="card" aria-labelledby="appearance-title">
          <div className="card-head">
            <h2 id="appearance-title">Appearance</h2>
          </div>
          <div className="card-body stack">
            <Segmented<ThemePref>
              label="Theme"
              value={pref}
              onChange={setPref}
              options={[
                { value: 'system', label: 'System', icon: Monitor },
                { value: 'light', label: 'Light', icon: Sun },
                { value: 'dark', label: 'Dark', icon: Moon },
              ]}
            />
            <p className="muted small">Stored in this browser only. Motion follows your system’s reduced-motion setting.</p>
          </div>
        </section>

        <section className="card" aria-labelledby="session-title">
          <div className="card-head">
            <h2 id="session-title">Admin session</h2>
          </div>
          <div className="card-body stack">
            <p className="small">
              {auth.mode === 'cookie'
                ? 'Signed in automatically because this browser is on the gateway machine. The session cookie is HttpOnly, scoped to the admin API, and expires after 12 hours.'
                : 'Signed in with the admin token, which is kept in this tab’s session storage and forgotten when the tab closes. The browser session expires after 12 hours; the stored token renews it automatically.'}
            </p>
            <div>
              <Button icon={LogOut} onClick={() => void auth.signOut()}>
                Sign out of this browser
              </Button>
            </div>
            <p className="muted xs">
              {auth.mode === 'cookie'
                ? 'Ends this browser’s session and clears cached data. On this machine you can sign back in with one click.'
                : 'Ends this browser’s session, removes the token from this tab and clears cached data.'}
            </p>
          </div>
        </section>

        <section className="card" aria-labelledby="about-title">
          <div className="card-head">
            <h2 id="about-title">About</h2>
          </div>
          <div className="card-body stack">
            <dl className="kv">
              <dt>Version</dt>
              <dd className="mono">{o?.version ?? '–'}</dd>
              <dt>Uptime</dt>
              <dd>{formatDuration(o?.uptime_seconds)}</dd>
            </dl>
            <a className="link small" href="https://github.com/RealSid08/switchyard" target="_blank" rel="noreferrer noopener">
              Source on GitHub <ExternalLink aria-hidden />
            </a>
          </div>
        </section>
      </div>
    </>
  );
}
