import { ArrowRight } from 'lucide-react';
import { useUsage, useUsageSources } from '../../app/queries';
import { Link } from '../../app/router';
import { KindMark } from '../../components/ui';
import { ApiError } from '../../lib/api';
import { costView, fmtMicros, fmtTokens, providerKind, tightestWindow, tokenKnowledge } from '../../lib/usage';

/** Overview strip: 24 h cost and tokens, and the one account window closest to its limit. */
export function UsageGlance() {
  const usage = useUsage({ window: '24h', source: 'gateway' });
  const sources = useUsageSources();
  if (usage.error instanceof ApiError && usage.error.isUnsupported) return null;
  const m = usage.data?.totals;
  const cost = m ? costView(m) : null;
  const tight = (sources.data?.sources ?? [])
    .filter((s) => s.status !== 'disabled')
    .map((s) => ({ s, t: tightestWindow(s) }))
    .filter((x): x is { s: (typeof x)['s']; t: NonNullable<(typeof x)['t']> } => !!x.t)
    .sort((a, b) => (b.t.meter.pct ?? 0) - (a.t.meter.pct ?? 0))[0];

  return (
    <section className="card usage-glance" aria-label="Usage in the last 24 hours">
      <Link to="/usage?window=24h" className="glance-cell">
        <span className="kpi-label">Estimated cost, 24 h</span>
        <span className="glance-value">{!m ? '…' : cost?.coverage === 'none' ? (m.units.total ? 'Not priced' : '$0.00') : fmtMicros(cost?.estimate)}</span>
        {cost?.unknownBillingUnits && cost.coverage !== 'none' ? (
          <span className="muted xs" title="Usage that doesn’t record whether an API key or a plan paid. Shown at API prices, not money charged.">
            incl. {fmtMicros(cost.unknownBilling) ?? '–'} billing unknown
          </span>
        ) : null}
      </Link>
      <Link to="/usage?window=24h" className="glance-cell">
        <span className="kpi-label">Tokens, 24 h</span>
        <span className="glance-value">{!m ? '…' : tokenKnowledge(m) === 'unknown' ? (m.units.total ? 'Unknown' : '0') : fmtTokens(m.tokens.total)}</span>
      </Link>
      <Link to="/usage/limits" className="glance-cell glance-limit">
        <span className="kpi-label">Closest to a limit</span>
        {tight ? (
          <span className="glance-limit-row">
            <KindMark kind={providerKind(tight.s.provider)} size="sm" />
            <span className="truncate">
              {tight.s.name} · {tight.t.window.label}
              {tight.t.window.model ? ` ${tight.t.window.model}` : ''}
            </span>
            <span className={`glance-pct tone-${tight.t.meter.tone}`}>{Math.round(tight.t.meter.pct ?? 0)}%</span>
          </span>
        ) : (
          <span className="glance-value small muted">{sources.isPending ? '…' : 'No plan limits reported'}</span>
        )}
      </Link>
      <Link to="/usage" className="glance-more" aria-label="Open usage">
        <ArrowRight aria-hidden />
      </Link>
    </section>
  );
}
