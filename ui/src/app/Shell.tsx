import {
  Activity,
  FlaskConical,
  KeyRound,
  LayoutDashboard,
  Menu as MenuIcon,
  Moon,
  Pause,
  Play,
  Plug,
  Search,
  Settings,
  SquareTerminal,
  Sun,
  TriangleAlert,
  Waypoints,
  type LucideIcon,
} from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { BrandMark } from '../components/BrandMark';
import { useConfirm, useToast } from '../components/feedback';
import { Button } from '../components/ui';
import { errorMessage } from '../lib/api';
import { formatNumber } from '../lib/format';
import { CommandPalette } from './CommandPalette';
import { useLiveStatus } from './live';
import { useConnections, useOverview, useSetPaused } from './queries';
import { Link, useLocation } from './router';
import { useTheme } from './theme';

export interface NavItem {
  to: string;
  label: string;
  icon: LucideIcon;
}

export const NAV: NavItem[] = [
  { to: '/', label: 'Overview', icon: LayoutDashboard },
  { to: '/connections', label: 'Connections', icon: Plug },
  { to: '/routes', label: 'Routes', icon: Waypoints },
  { to: '/activity', label: 'Activity', icon: Activity },
  { to: '/playground', label: 'Playground', icon: FlaskConical },
  { to: '/clients', label: 'Connect clients', icon: SquareTerminal },
  { to: '/keys', label: 'API keys', icon: KeyRound },
];

function useMediaQuery(query: string) {
  const [match, setMatch] = useState(() => window.matchMedia(query).matches);
  useEffect(() => {
    const mq = window.matchMedia(query);
    const on = () => setMatch(mq.matches);
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }, [query]);
  return match;
}

function isActive(path: string, to: string) {
  return to === '/' ? path === '/' : path === to || path.startsWith(`${to}/`);
}

export function Shell({ children }: { children: ReactNode }) {
  const { path } = useLocation();
  // The drawer remembers which page it was opened on, so navigating closes it.
  const [navOpenAt, setNavOpenAt] = useState<string | null>(null);
  const navOpen = navOpenAt === path;
  const setNavOpen = (open: boolean) => setNavOpenAt(open ? path : null);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const overview = useOverview();
  const first = useRef(true);
  const narrow = useMediaQuery('(max-width: 900px)');

  // Close the drawer on navigation, move focus to the new page's heading, and
  // keep the document title in sync (announced by screen readers).
  useEffect(() => {
    const item = NAV.find((n) => isActive(path, n.to)) ?? (path.startsWith('/settings') ? { label: 'Settings' } : null);
    document.title = item ? `${item.label} · Switchyard` : 'Switchyard';
    if (first.current) {
      first.current = false;
      return;
    }
    window.scrollTo({ top: 0 });
    requestAnimationFrame(() => document.querySelector<HTMLElement>('[data-page-title]')?.focus({ preventScroll: true }));
  }, [path]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPaletteOpen((o) => !o);
      }
      if (e.key === 'Escape') setNavOpenAt(null);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const paused = overview.data?.paused ?? false;

  return (
    <div className="shell" data-nav-open={navOpen}>
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <Sidebar path={path} onOpenPalette={() => setPaletteOpen(true)} hidden={narrow && !navOpen} />
      {navOpen ? <div className="nav-scrim" onClick={() => setNavOpen(false)} aria-hidden /> : null}
      <div className="main">
        <header className="topbar">
          <Button variant="ghost" iconOnly icon={MenuIcon} aria-label="Open navigation" aria-expanded={navOpen} onClick={() => setNavOpen(true)} />
          <Link to="/" className="brand">
            <BrandMark />
            <span className="brand-name">Switchyard</span>
          </Link>
          <LivePill />
          <Button variant="ghost" iconOnly icon={Search} aria-label="Open command palette" onClick={() => setPaletteOpen(true)} />
        </header>
        {paused ? <PausedBanner /> : null}
        <main id="main" className="page" tabIndex={-1}>
          {children}
        </main>
      </div>
      <CommandPalette open={paletteOpen} onClose={() => setPaletteOpen(false)} />
    </div>
  );
}

function Sidebar({ path, onOpenPalette, hidden }: { path: string; onOpenPalette: () => void; hidden: boolean }) {
  const overview = useOverview();
  const connections = useConnections();
  const { resolved, setPref } = useTheme();
  const conns = connections.data;
  return (
    <aside className="sidebar" aria-label="Sidebar" inert={hidden}>
      <Link to="/" className="brand" aria-label="Switchyard overview">
        <BrandMark />
        <span className="brand-name">Switchyard</span>
        {overview.data?.version ? <span className="brand-version">v{overview.data.version}</span> : null}
      </Link>
      <button type="button" className="btn btn-sm" style={{ justifyContent: 'flex-start', color: 'var(--text-muted)', margin: '0 0 10px' }} onClick={onOpenPalette}>
        <Search aria-hidden />
        <span>Jump to…</span>
        <span className="spacer" />
        <kbd>⌘K</kbd>
      </button>
      <nav className="nav" aria-label="Main">
        {NAV.map((item) => (
          <Link key={item.to} to={item.to} aria-current={isActive(path, item.to) ? 'page' : undefined}>
            <item.icon aria-hidden />
            {item.label}
            {item.to === '/connections' && conns ? (
              <span className="nav-meta" aria-label={`${conns.filter((c) => c.enabled).length} of ${conns.length} enabled`}>
                {conns.length ? `${conns.filter((c) => c.enabled).length}/${conns.length}` : null}
              </span>
            ) : null}
            {item.to === '/activity' && overview.data?.active_requests ? (
              <span className="nav-meta">
                <span className="dot dot-info" aria-hidden />
                {formatNumber(overview.data.active_requests)} <span className="sr-only">in flight</span>
              </span>
            ) : null}
          </Link>
        ))}
        <Link to="/settings" aria-current={isActive(path, '/settings') ? 'page' : undefined}>
          <Settings aria-hidden />
          Settings
        </Link>
      </nav>
      <div className="sidebar-foot">
        <GatewayCard />
        <div className="sidebar-tools">
          <LivePill />
          <span className="spacer" />
          <Button
            variant="ghost"
            size="sm"
            iconOnly
            icon={resolved === 'dark' ? Sun : Moon}
            aria-label={`Switch to ${resolved === 'dark' ? 'light' : 'dark'} theme`}
            title={`Switch to ${resolved === 'dark' ? 'light' : 'dark'} theme`}
            onClick={() => setPref(resolved === 'dark' ? 'light' : 'dark')}
          />
        </div>
      </div>
    </aside>
  );
}

export function usePauseToggle() {
  const setPaused = useSetPaused();
  const confirm = useConfirm();
  const toast = useToast();
  return {
    pending: setPaused.isPending,
    async toggle(next: boolean) {
      if (next) {
        const ok = await confirm({
          title: 'Pause the gateway?',
          message: 'New client requests will be refused with 503 until you resume. Requests already in flight finish normally. The dashboard keeps working.',
          confirmLabel: 'Pause gateway',
        });
        if (!ok) return;
      }
      setPaused.mutate(next, {
        onSuccess: () => toast({ tone: next ? 'info' : 'ok', title: next ? 'Gateway paused' : 'Gateway resumed', message: next ? 'Clients will get 503 until you resume.' : 'Traffic is flowing again.' }),
        onError: (e) => toast({ tone: 'err', title: next ? "Couldn't pause" : "Couldn't resume", message: errorMessage(e) }),
      });
    },
  };
}

function GatewayCard() {
  const overview = useOverview();
  const pause = usePauseToggle();
  const o = overview.data;
  if (!o) {
    return (
      <div className="gateway-card" aria-busy>
        <span className="skeleton" style={{ height: 14, width: '60%' }} />
        <span className="skeleton" style={{ height: 28 }} />
      </div>
    );
  }
  return (
    <section className="gateway-card" aria-label="Gateway status">
      <div className="state">
        <span className={`dot ${o.paused ? 'dot-warn' : 'dot-ok dot-pulse'}`} aria-hidden />
        {o.paused ? 'Paused' : 'Accepting traffic'}
      </div>
      <div className="detail">
        {o.paused
          ? 'Clients get 503 until you resume.'
          : o.active_requests
            ? `${formatNumber(o.active_requests)} in flight`
            : o.connections_total
              ? `${formatNumber(o.connections_enabled)} of ${formatNumber(o.connections_total)} connections enabled`
              : 'No connections yet'}
      </div>
      <Button size="sm" variant={o.paused ? 'primary' : 'default'} icon={o.paused ? Play : Pause} loading={pause.pending} onClick={() => pause.toggle(!o.paused)}>
        {o.paused ? 'Resume gateway' : 'Pause gateway'}
      </Button>
    </section>
  );
}

function PausedBanner() {
  const pause = usePauseToggle();
  return (
    <div className="paused-banner" role="status">
      <TriangleAlert aria-hidden />
      <span>
        <strong>Gateway paused.</strong> New client requests are refused with 503 until you resume.
      </span>
      <Button size="sm" variant="primary" icon={Play} loading={pause.pending} onClick={() => pause.toggle(false)}>
        Resume
      </Button>
    </div>
  );
}

const LIVE_LABEL = {
  live: 'Live',
  connecting: 'Connecting…',
  reconnecting: 'Reconnecting',
  polling: 'Polling',
  offline: 'Offline',
  stopped: 'Idle',
} as const;

export function LivePill() {
  const { status, nextRetryAt, retry } = useLiveStatus();
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!nextRetryAt) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [nextRetryAt]);
  // `now` can lag a tick behind a fresh retry time; the backoff never exceeds 30 s.
  const secs = nextRetryAt ? Math.min(30, Math.max(0, Math.ceil((nextRetryAt - now) / 1000))) : null;
  const dot = status === 'live' ? 'dot-ok dot-pulse' : status === 'offline' ? 'dot-err' : status === 'polling' || status === 'reconnecting' ? 'dot-warn' : '';
  const detail =
    status === 'live'
      ? 'Live updates over WebSocket.'
      : status === 'polling'
        ? `Live socket unavailable; refreshing every 5 s.${secs !== null ? ` Retrying socket in ${secs}s.` : ''} Click to retry now.`
        : status === 'reconnecting'
          ? `Live connection dropped.${secs !== null ? ` Retrying in ${secs}s.` : ''} Click to retry now.`
          : status === 'offline'
            ? 'Your browser is offline.'
            : 'Connecting to live updates.';
  const label = status === 'reconnecting' && secs !== null ? `Retry in ${secs}s` : LIVE_LABEL[status];
  const clickable = status === 'reconnecting' || status === 'polling';
  const content = (
    <>
      <span className={`dot ${dot}`} aria-hidden />
      {label}
    </>
  );
  return clickable ? (
    <button type="button" className="live-pill" onClick={retry} title={detail} aria-label={`Live updates: ${detail}`}>
      {content}
    </button>
  ) : (
    <span className="live-pill" title={detail} aria-label={`Live updates: ${detail}`}>
      {content}
    </span>
  );
}
