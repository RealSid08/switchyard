import { CornerDownLeft, KeyRound, Moon, Pause, Play, Plug, Plus, Search, Settings, Sun, Waypoints, type LucideIcon } from 'lucide-react';
import { useMemo, useState, type KeyboardEvent } from 'react';
import { Dialog } from '../components/Dialog';
import { useOverview, useRoutes, useModels } from './queries';
import { navigate } from './router';
import { NAV, usePauseToggle } from './Shell';
import { useTheme } from './theme';

interface Command {
  id: string;
  label: string;
  hint?: string;
  icon: LucideIcon;
  run: () => void;
  keywords?: string;
}

/** Fuzzy-ish match: every query character appears in order. Earlier, tighter matches rank higher. */
export function score(text: string, query: string): number {
  const t = text.toLowerCase();
  const q = query.toLowerCase().trim();
  if (!q) return 1;
  if (t.includes(q)) return 100 - t.indexOf(q);
  let ti = 0;
  let gaps = 0;
  for (const ch of q) {
    const found = t.indexOf(ch, ti);
    if (found === -1) return 0;
    gaps += found - ti;
    ti = found + 1;
  }
  return Math.max(1, 50 - gaps);
}

export function CommandPalette({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Dialog open={open} onClose={onClose} title="Jump to" width={560}>
      <PaletteBody onClose={onClose} />
    </Dialog>
  );
}

function PaletteBody({ onClose }: { onClose: () => void }) {
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const overview = useOverview();
  const routes = useRoutes();
  const models = useModels();
  const pause = usePauseToggle();
  const { resolved, setPref } = useTheme();

  const commands = useMemo<Command[]>(() => {
    const go = (to: string) => () => navigate(to);
    const list: Command[] = [
      ...NAV.map((n) => ({ id: `nav-${n.to}`, label: n.label, hint: 'Go to', icon: n.icon, run: go(n.to) })),
      { id: 'nav-settings', label: 'Settings', hint: 'Go to', icon: Settings, run: go('/settings') },
      { id: 'add-conn', label: 'Add a connection', hint: 'Action', icon: Plus, run: go('/connections?new=1'), keywords: 'provider openai anthropic gemini' },
      { id: 'import', label: 'Import Codex or Claude login', hint: 'Action', icon: Plug, run: go('/connections?import=1'), keywords: 'credentials cliproxy' },
      { id: 'new-route', label: 'Create a model route', hint: 'Action', icon: Waypoints, run: go('/routes?new=1') },
      { id: 'new-key', label: 'Create a client key', hint: 'Action', icon: KeyRound, run: go('/keys?new=1') },
      {
        id: 'theme',
        label: `Switch to ${resolved === 'dark' ? 'light' : 'dark'} theme`,
        hint: 'Action',
        icon: resolved === 'dark' ? Sun : Moon,
        run: () => setPref(resolved === 'dark' ? 'light' : 'dark'),
      },
    ];
    if (overview.data) {
      const paused = overview.data.paused;
      list.push({ id: 'pause', label: paused ? 'Resume gateway' : 'Pause gateway', hint: 'Action', icon: paused ? Play : Pause, run: () => void pause.toggle(!paused) });
    }
    const seen = new Set<string>();
    for (const r of routes.data ?? []) {
      seen.add(r.model);
      list.push({ id: `pg-${r.model}`, label: r.model, hint: 'Try in playground', icon: Waypoints, run: go(`/playground?model=${encodeURIComponent(r.model)}`) });
    }
    for (const m of models.data ?? []) {
      if (seen.has(m.id)) continue;
      seen.add(m.id);
      list.push({ id: `pg-${m.id}`, label: m.id, hint: 'Try in playground', icon: Search, run: go(`/playground?model=${encodeURIComponent(m.id)}`), keywords: m.connection_name });
    }
    return list;
  }, [overview.data, routes.data, models.data, resolved, setPref, pause]);

  const results = useMemo(() => {
    if (!query.trim()) return commands.slice(0, 14);
    return commands
      .map((c) => ({ c, s: Math.max(score(c.label, query), score(`${c.hint ?? ''} ${c.keywords ?? ''}`, query) * 0.5) }))
      .filter((x) => x.s > 0)
      .sort((a, b) => b.s - a.s)
      .slice(0, 14)
      .map((x) => x.c);
  }, [commands, query]);

  const runAt = (i: number) => {
    const c = results[i];
    if (!c) return;
    onClose();
    c.run();
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setActive((a) => Math.min(results.length - 1, a + 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActive((a) => Math.max(0, a - 1));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      runAt(active);
    }
  };

  const activeId = results[active] ? `cmd-${results[active].id}` : undefined;
  return (
    <div className="palette" onKeyDown={onKey}>
      <div className="input-group">
        <Search className="input-icon" aria-hidden />
        <input
          className="input has-icon"
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-activedescendant={activeId}
          aria-label="Search pages, actions and models"
          placeholder="Search pages, actions and models…"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setActive(0);
          }}
          data-autofocus
          autoComplete="off"
          spellCheck={false}
        />
      </div>
      <ul id="palette-list" role="listbox" aria-label="Results" className="palette-list">
        {results.length === 0 ? <li className="muted small palette-empty">No matches.</li> : null}
        {results.map((c, i) => (
          <li
            key={c.id}
            id={`cmd-${c.id}`}
            role="option"
            aria-selected={i === active}
            className="palette-item"
            onMouseMove={() => setActive(i)}
            onClick={() => runAt(i)}
          >
            <c.icon aria-hidden />
            <span className="truncate">{c.label}</span>
            <span className="spacer" />
            <span className="muted xs">{c.hint}</span>
            {i === active ? <CornerDownLeft className="muted" aria-hidden width={13} height={13} /> : null}
          </li>
        ))}
      </ul>
    </div>
  );
}
