import { useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { niceMax } from './TrafficChart';

export interface StackSeries {
  key: string;
  label: string;
  color: string;
  /** Drawn with diagonal stripes: a quantity whose kind isn't known (e.g. billing unknown). */
  hatch?: boolean;
}

/** Legend and tooltip swatch; hatched series get the same stripes as their bars. */
export function swatchStyle(s: Pick<StackSeries, 'color' | 'hatch'>): CSSProperties {
  return { background: s.hatch ? `repeating-linear-gradient(135deg, ${s.color} 0 3px, transparent 3px 5px)` : s.color };
}

export interface StackBucket {
  label: string;
  /** Full label for tooltips and the data table. */
  title: string;
  values: Record<string, number>;
  /** Nothing in this bucket was measurable (drawn hatched, not as zero). */
  unknown?: boolean;
}

/**
 * Stacked columns for usage series. One axis, thin marks, 2px gaps between
 * segments, a legend, hover tooltip and a screen-reader table.
 */
export function StackedBars({
  buckets,
  series,
  format,
  height = 200,
  ariaLabel,
}: {
  buckets: StackBucket[];
  series: StackSeries[];
  format: (v: number) => string;
  height?: number;
  ariaLabel: string;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(640);
  const [hover, setHover] = useState<number | null>(null);
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setWidth(Math.max(260, Math.round(e.contentRect.width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const totals = useMemo(() => buckets.map((b) => series.reduce((s, x) => s + (b.values[x.key] ?? 0), 0)), [buckets, series]);
  const max = niceMax(Math.max(0, ...totals));
  const padL = 52;
  const padR = 8;
  const padT = 10;
  const padB = 24;
  const plotW = width - padL - padR;
  const plotH = height - padT - padB;
  const slot = buckets.length ? plotW / buckets.length : plotW;
  const colW = Math.max(2, Math.min(24, slot - 2));
  const y = (v: number) => padT + plotH - (v / max) * plotH;
  const ticks = [0, max / 2, max];
  const labelEvery = Math.max(1, Math.ceil(buckets.length / Math.max(2, Math.floor(plotW / 70))));
  const hb = hover !== null ? buckets[hover] : null;

  return (
    <div className="chart" ref={wrap}>
      <div className="legend chart-legend" aria-hidden>
        {series.map((s) => (
          <span key={s.key}>
            <span className="swatch" style={swatchStyle(s)} /> {s.label}
          </span>
        ))}
      </div>
      <svg width={width} height={height} role="img" aria-label={ariaLabel} onMouseLeave={() => setHover(null)}>
        <defs>
          <pattern id="unknown-hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <rect width="6" height="6" fill="transparent" />
            <line x1="0" y1="0" x2="0" y2="6" stroke="var(--border-strong)" strokeWidth="2" />
          </pattern>
          <pattern id="series-hatch" width="5" height="5" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
            <line x1="0" y1="0" x2="0" y2="5" stroke="var(--surface)" strokeWidth="2" />
          </pattern>
        </defs>
        {ticks.map((t) => (
          <g key={t}>
            <line x1={padL} x2={width - padR} y1={y(t)} y2={y(t)} stroke="var(--chart-grid)" strokeWidth={1} />
            <text x={padL - 8} y={y(t)} dy="0.32em" textAnchor="end" className="chart-tick">
              {format(t)}
            </text>
          </g>
        ))}
        {buckets.map((b, i) => {
          const x = padL + i * slot + (slot - colW) / 2;
          let base = padT + plotH;
          const segs = series
            .map((s) => ({ s, v: b.values[s.key] ?? 0 }))
            .filter((x) => x.v > 0)
            .map(({ s, v }, j, arr) => {
              const h = Math.max(1, (v / max) * plotH - (j < arr.length - 1 ? 2 : 0));
              const top = base - h;
              const rx = j === arr.length - 1 ? Math.min(3, colW / 2) : 0;
              const el = (
                <g key={s.key}>
                  <rect x={x} y={top} width={colW} height={h} rx={rx} fill={s.color} />
                  {s.hatch ? <rect x={x} y={top} width={colW} height={h} rx={rx} fill="url(#series-hatch)" /> : null}
                </g>
              );
              base = top - 2;
              return el;
            });
          return (
            <g key={b.title} opacity={hover === null || hover === i ? 1 : 0.5}>
              {b.unknown && totals[i] === 0 ? <rect x={x} y={padT + plotH - 18} width={colW} height={18} fill="url(#unknown-hatch)" /> : segs}
              <rect x={padL + i * slot} y={padT} width={slot} height={plotH} fill="transparent" onMouseEnter={() => setHover(i)} />
            </g>
          );
        })}
        {buckets.map((b, i) =>
          i % labelEvery === 0 || i === buckets.length - 1 ? (
            <text key={`l-${b.title}`} x={padL + i * slot + slot / 2} y={height - 6} textAnchor="middle" className="chart-tick">
              {i % labelEvery === 0 ? b.label : ''}
            </text>
          ) : null,
        )}
      </svg>
      {hb && hover !== null ? (
        <div className="chart-tip" style={{ left: Math.min(width - 190, Math.max(0, padL + hover * slot - 70)), top: 30, width: 190 }} aria-hidden>
          <div className="muted xs">{hb.title}</div>
          {hb.unknown && totals[hover] === 0 ? (
            <div className="xs">No measured usage in this period</div>
          ) : (
            <>
              {series.map((s) => (
                <div className="row" key={s.key}>
                  <span className="swatch" style={swatchStyle(s)} /> {s.label}
                  <span className="spacer" />
                  <span className="num">{format(hb.values[s.key] ?? 0)}</span>
                </div>
              ))}
              {series.length > 1 ? (
                <div className="row tip-total">
                  Total
                  <span className="spacer" />
                  <span className="num">{format(totals[hover])}</span>
                </div>
              ) : null}
            </>
          )}
        </div>
      ) : null}
      <div className="sr-only">
        <table>
          <caption>{ariaLabel}</caption>
          <thead>
            <tr>
              <th>Period</th>
              {series.map((s) => (
                <th key={s.key}>{s.label}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {buckets.map((b, i) => (
              <tr key={b.title}>
                <td>{b.title}</td>
                {series.map((s) => (
                  <td key={s.key}>{b.unknown && totals[i] === 0 ? 'Unknown' : format(b.values[s.key] ?? 0)}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
