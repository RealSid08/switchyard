import { useEffect, useMemo, useRef, useState } from 'react';
import { formatNumber, formatTime, toMillis } from '../lib/format';
import type { SeriesPoint } from '../lib/types';

export interface Bucket {
  start: number;
  requests: number;
  errors: number;
}

/**
 * Lay the server's sparse per-minute series onto a continuous window ending now,
 * so quiet minutes show as gaps rather than being silently skipped.
 */
export function bucketize(series: SeriesPoint[], now: number, minutes = 60): Bucket[] {
  const step = 60_000;
  const end = Math.floor(now / step) * step;
  const start = end - (minutes - 1) * step;
  const out: Bucket[] = Array.from({ length: minutes }, (_, i) => ({ start: start + i * step, requests: 0, errors: 0 }));
  for (const p of series) {
    const t = Math.floor(toMillis(p.timestamp) / step) * step;
    if (t < start || t > end) continue;
    const b = out[(t - start) / step];
    b.requests += Math.max(0, p.requests || 0);
    b.errors += Math.max(0, Math.min(p.errors || 0, p.requests || 0));
  }
  return out;
}

export function niceMax(n: number): number {
  if (n <= 4) return 4;
  const pow = 10 ** Math.floor(Math.log10(n));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * pow >= n) return m * pow;
  return 10 * pow;
}

/** A column path with a 4px rounded top and a square base. */
function colPath(x: number, y: number, w: number, h: number, round: boolean) {
  if (h <= 0) return '';
  const r = round ? Math.min(4, w / 2, h) : 0;
  return `M${x},${y + h}V${y + r}${r ? `Q${x},${y} ${x + r},${y}` : ''}H${x + w - r}${r ? `Q${x + w},${y} ${x + w},${y + r}` : ''}V${y + h}Z`;
}

export function TrafficChart({ buckets, height = 180 }: { buckets: Bucket[]; height?: number }) {
  const wrap = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(640);
  const [hover, setHover] = useState<number | null>(null);

  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(([entry]) => setWidth(Math.max(240, Math.round(entry.contentRect.width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const padL = 34;
  const padR = 8;
  const padT = 10;
  const padB = 22;
  const plotW = width - padL - padR;
  const plotH = height - padT - padB;
  const max = useMemo(() => niceMax(Math.max(0, ...buckets.map((b) => b.requests))), [buckets]);
  const slot = plotW / buckets.length;
  const colW = Math.max(2, Math.min(24, slot - 2));
  const y = (v: number) => padT + plotH - (v / max) * plotH;
  const ticks = [0, max / 2, max];
  const labelIdx = [0, Math.floor(buckets.length / 2), buckets.length - 1];
  const hb = hover !== null ? buckets[hover] : null;

  return (
    <div className="chart" ref={wrap}>
      <svg width={width} height={height} role="img" aria-label="Requests per minute over the last hour, split into successful and failed requests" onMouseLeave={() => setHover(null)}>
        {ticks.map((t) => (
          <g key={t}>
            <line x1={padL} x2={width - padR} y1={y(t)} y2={y(t)} stroke="var(--chart-grid)" strokeWidth={1} />
            <text x={padL - 8} y={y(t)} dy="0.32em" textAnchor="end" className="chart-tick">
              {formatNumber(t)}
            </text>
          </g>
        ))}
        {buckets.map((b, i) => {
          const x = padL + i * slot + (slot - colW) / 2;
          const ok = b.requests - b.errors;
          const okH = (ok / max) * plotH;
          const errH = (b.errors / max) * plotH;
          const gap = ok > 0 && b.errors > 0 ? 2 : 0;
          const base = padT + plotH;
          return (
            <g key={b.start} opacity={hover === null || hover === i ? 1 : 0.55}>
              <path d={colPath(x, base - okH, colW, okH, b.errors === 0)} fill="var(--chart-primary)" />
              <path d={colPath(x, base - okH - gap - errH, colW, errH, true)} fill="var(--chart-error)" />
              <rect x={padL + i * slot} y={padT} width={slot} height={plotH} fill="transparent" onMouseEnter={() => setHover(i)} />
            </g>
          );
        })}
        {labelIdx.map((i, n) => (
          <text key={i} x={padL + i * slot + slot / 2} y={height - 6} textAnchor={n === 0 ? 'start' : n === 2 ? 'end' : 'middle'} className="chart-tick">
            {n === 2 ? 'now' : formatTime(buckets[i].start).replace(/:\d\d(\s|$)/, '$1')}
          </text>
        ))}
      </svg>
      {hb ? (
        <div
          className="chart-tip"
          style={{
            left: Math.min(width - 150, Math.max(0, padL + (hover ?? 0) * slot - 60)),
            top: 0,
          }}
          aria-hidden
        >
          <div className="muted xs">{formatTime(hb.start)}</div>
          <div className="row">
            <span className="swatch" style={{ background: 'var(--chart-primary)' }} /> Succeeded
            <span className="spacer" />
            <span className="num">{formatNumber(hb.requests - hb.errors)}</span>
          </div>
          <div className="row">
            <span className="swatch" style={{ background: 'var(--chart-error)' }} /> Failed
            <span className="spacer" />
            <span className="num">{formatNumber(hb.errors)}</span>
          </div>
        </div>
      ) : null}
      <div className="sr-only">
      <table>
        <caption>Requests per minute, last hour (minutes with traffic)</caption>
        <thead>
          <tr>
            <th>Minute</th>
            <th>Requests</th>
            <th>Errors</th>
          </tr>
        </thead>
        <tbody>
          {buckets
            .filter((b) => b.requests > 0)
            .map((b) => (
              <tr key={b.start}>
                <td>{formatTime(b.start)}</td>
                <td>{b.requests}</td>
                <td>{b.errors}</td>
              </tr>
            ))}
        </tbody>
      </table>
      </div>
    </div>
  );
}
