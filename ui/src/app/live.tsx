import { useQueryClient } from '@tanstack/react-query';
import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { EventsClient, eventsUrl, type LiveStatus } from '../lib/events';
import type { Overview } from '../lib/types';
import { applyRequestEvent, qk } from './queries';

interface LiveState {
  status: LiveStatus;
  nextRetryAt: number | null;
  retry: () => void;
}

const LiveContext = createContext<LiveState>({ status: 'stopped', nextRetryAt: null, retry: () => {} });

export function useLiveStatus() {
  return useContext(LiveContext);
}

/**
 * Owns the /api/events socket. Pushed records and overview snapshots go straight
 * into the query cache; when the socket is down, queries fall back to polling
 * (see usePollInterval) and we keep retrying with backoff in the background.
 */
export function LiveProvider({ children, enabled }: { children: ReactNode; enabled: boolean }) {
  const qc = useQueryClient();
  const [status, setStatus] = useState<LiveStatus>('stopped');
  const [nextRetryAt, setNextRetryAt] = useState<number | null>(null);
  const clientRef = useRef<EventsClient | null>(null);

  useEffect(() => {
    if (!enabled) return;
    let lastOverviewAt = 0;
    let wasDown = false;
    let overviewTimer: ReturnType<typeof setTimeout> | null = null;
    const client = new EventsClient({
      url: eventsUrl(window.location),
      onStatus: (s, info) => {
        setStatus(s);
        setNextRetryAt(info.nextRetryMs === null ? null : Date.now() + info.nextRetryMs);
        // Coming back after a gap: anything could have changed while we were away.
        if (s === 'live' && wasDown) void qc.invalidateQueries();
        if (s === 'reconnecting' || s === 'polling' || s === 'offline') wasDown = true;
        if (s === 'live') wasDown = false;
      },
      onMessage: (msg) => {
        if (msg.type === 'overview') {
          lastOverviewAt = Date.now();
          qc.setQueryData<Overview>(qk.overview, msg.data);
          return;
        }
        applyRequestEvent(qc, msg.data);
        // If the server isn't pushing overview snapshots, refresh counters (throttled).
        if (Date.now() - lastOverviewAt > 3000 && !overviewTimer) {
          overviewTimer = setTimeout(() => {
            overviewTimer = null;
            void qc.invalidateQueries({ queryKey: qk.overview });
          }, 1500);
        }
      },
    });
    clientRef.current = client;
    // Defer a tick so StrictMode's dev-only mount/unmount/mount doesn't open and
    // immediately abort a socket.
    const startTimer = setTimeout(() => client.start(), 0);

    const online = () => client.retryNow();
    const offline = () => client.markOffline();
    const visible = () => {
      if (document.visibilityState === 'visible') client.retryNow();
    };
    window.addEventListener('online', online);
    window.addEventListener('offline', offline);
    document.addEventListener('visibilitychange', visible);
    return () => {
      window.removeEventListener('online', online);
      window.removeEventListener('offline', offline);
      document.removeEventListener('visibilitychange', visible);
      clearTimeout(startTimer);
      if (overviewTimer) clearTimeout(overviewTimer);
      client.stop();
      clientRef.current = null;
    };
  }, [enabled, qc]);

  const value = useMemo<LiveState>(
    () => ({ status: enabled ? status : 'stopped', nextRetryAt, retry: () => clientRef.current?.retryNow() }),
    [enabled, status, nextRetryAt],
  );
  return <LiveContext.Provider value={value}>{children}</LiveContext.Provider>;
}
