import { useQueryClient } from '@tanstack/react-query';
import { FlaskConical, X } from 'lucide-react';
import { useState } from 'react';
import './mock.css';

/**
 * Dev-only controls for the mock backend (`pnpm dev:mock`). Only loaded when
 * VITE_SWITCHYARD_MOCK is set, which the production build refuses to do.
 */
export default function MockPanel() {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const call = async (action: string) => {
    setBusy(action);
    try {
      await fetch(`/api/__mock/${action}`, { method: 'POST', credentials: 'include' });
      await qc.invalidateQueries();
    } finally {
      setBusy(null);
    }
  };
  const actions: [string, string][] = [
    ['seed', 'Seed sample data'],
    ['traffic-on', 'Simulate traffic'],
    ['traffic-off', 'Stop traffic'],
    ['burst', 'Burst of 25 requests'],
    ['drop-events', 'Drop live sockets'],
    ['events-off', 'Refuse live sockets'],
    ['events-on', 'Accept live sockets'],
    ['expire-session', 'Expire admin session'],
    ['reset', 'Reset to empty'],
  ];
  return (
    <div className="mock-panel" data-open={open}>
      {open ? (
        <div className="mock-panel-body" role="region" aria-label="Mock backend controls">
          <div className="row">
            <strong className="small">Mock backend</strong>
            <span className="spacer" />
            <button type="button" className="btn btn-ghost btn-sm btn-icon" aria-label="Close mock controls" onClick={() => setOpen(false)}>
              <X aria-hidden />
            </button>
          </div>
          {actions.map(([id, label]) => (
            <button key={id} type="button" className="btn btn-sm" disabled={!!busy} onClick={() => void call(id)}>
              {busy === id ? '…' : label}
            </button>
          ))}
          <span className="muted xs">Dev only. Never in production builds.</span>
        </div>
      ) : (
        <button type="button" className="btn btn-sm mock-toggle" onClick={() => setOpen(true)}>
          <FlaskConical aria-hidden /> Mock
        </button>
      )}
    </div>
  );
}
