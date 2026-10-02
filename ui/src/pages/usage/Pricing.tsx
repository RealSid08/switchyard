import { useQueryClient } from '@tanstack/react-query';
import { ExternalLink, Plus, RefreshCw, Search, TriangleAlert, X } from 'lucide-react';
import { useMemo, useState } from 'react';
import { qk, usePricing } from '../../app/queries';
import { useToast } from '../../components/feedback';
import { Badge, Button, Callout, KindMark, Skeleton } from '../../components/ui';
import { ApiError, errorMessage } from '../../lib/api';
import { api } from '../../lib/client';
import { formatNumber, toDate } from '../../lib/format';
import { providerKind, providerLabel, rateError } from '../../lib/usage';
import type { PriceOverride, PriceRate, PricingTable, RateSet } from '../../lib/usageTypes';

/** Editable columns. "Cache write" is the 5 minute tier where a provider has tiers, else its single rate. */
type RateKey = 'input' | 'output' | 'cache_read' | 'cache_write' | 'cache_write_1h';
const RATE_COLS: { key: RateKey; label: string; short: string; hint?: string }[] = [
  { key: 'input', label: 'Input', short: 'In' },
  { key: 'output', label: 'Output', short: 'Out' },
  { key: 'cache_read', label: 'Cache read', short: 'Cache read' },
  { key: 'cache_write', label: 'Cache write', short: 'Write', hint: 'The 5 minute cache tier, or the only cache-write price if the provider has one.' },
  { key: 'cache_write_1h', label: 'Cache write (1 h)', short: 'Write 1h' },
];

const rateOf = (r: RateSet, k: RateKey) => (k === 'cache_write' ? (r.cache_write_5m ?? r.cache_write) : r[k]);

const dateFmt = new Intl.DateTimeFormat(undefined, { year: 'numeric', month: 'short', day: 'numeric', timeZone: 'UTC' });
const fmtDate = (s: string | null | undefined) => {
  const d = toDate(s ?? null);
  return d ? dateFmt.format(d) : (s ?? '');
};

/** Exact list price: at least 2 decimals, never rounded away (0.125 stays 0.125). */
export function fmtRate(v: string | null | undefined) {
  if (v === null || v === undefined || v === '') return null;
  const s = String(v).trim();
  if (!/^\d+(\.\d+)?$/.test(s)) return s;
  const [i, d = ''] = s.split('.');
  const frac = d.replace(/0+$/, '').padEnd(2, '0');
  return `$${Number(i).toLocaleString('en-US')}.${frac}`;
}

interface DraftRow {
  key: number;
  model: string;
  rates: Record<RateKey, string>;
}

let seq = 0;
const emptyRates = (): Record<RateKey, string> => ({ input: '', output: '', cache_read: '', cache_write: '', cache_write_1h: '' });

function toDraft(t: PricingTable): DraftRow[] {
  return t.rates
    .filter((r) => r.origin === 'override')
    .map((r) => ({
      key: ++seq,
      model: r.model,
      rates: Object.fromEntries(RATE_COLS.map((c) => [c.key, rateOf(r.usd_per_mtok, c.key) ?? ''])) as Record<RateKey, string>,
    }));
}

export function PricingTab() {
  const pricing = usePricing();
  if (pricing.error instanceof ApiError && pricing.error.isUnsupported) {
    return (
      <Callout tone="info" title="This gateway doesn’t price usage yet">
        Update Switchyard to see the list prices behind cost estimates and set your own.
      </Callout>
    );
  }
  if (pricing.isPending) {
    return (
      <div className="stack" role="status" aria-busy aria-label="Loading prices">
        <Skeleton h={60} />
        <Skeleton h={240} />
      </div>
    );
  }
  if (!pricing.data) {
    return (
      <Callout tone="err" title="Couldn’t load prices" role="alert" action={<Button size="sm" icon={RefreshCw} onClick={() => pricing.refetch()}>Retry</Button>}>
        {errorMessage(pricing.error)}
      </Callout>
    );
  }
  return <PricingBody t={pricing.data} />;
}

function PricingBody({ t }: { t: PricingTable }) {
  // Server overrides until you start editing; then your edits until you save or discard.
  const base = useMemo(() => toDraft(t), [t]);
  const [edits, setEdits] = useState<DraftRow[] | null>(null);
  const draft = edits ?? base;
  const dirty = edits !== null;
  const [query, setQuery] = useState('');
  const [provider, setProvider] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const qc = useQueryClient();
  const toast = useToast();

  const edit = (rows: DraftRow[]) => setEdits(rows);
  const addRow = (model = '') => {
    if (model && draft.some((d) => d.model === model)) return;
    edit([...draft, { key: ++seq, model, rates: emptyRates() }]);
    requestAnimationFrame(() => document.querySelector<HTMLInputElement>(`#ovr-${seq}-${model ? 'input' : 'model'}`)?.focus());
  };

  const problems = draft.flatMap((d) => {
    const out: string[] = [];
    if (!d.model.trim()) out.push('Every row needs a model name.');
    if (d.model.length > 200) out.push(`${d.model.slice(0, 20)}…: model name is too long.`);
    if (!d.rates.input.trim() || !d.rates.output.trim()) out.push(`${d.model || 'New row'}: set at least input and output prices.`);
    return out;
  });
  const dupes = draft.map((d) => d.model.trim()).filter((m, i, a) => m && a.indexOf(m) !== i);
  const fieldErrors = draft.some((d) => RATE_COLS.some((c) => rateError(d.rates[c.key])));
  const canSave = dirty && !problems.length && !dupes.length && !fieldErrors && draft.length <= 200;

  const save = async () => {
    setSaving(true);
    setError(null);
    const v = (s: string) => s.trim() || null;
    const overrides: PriceOverride[] = draft.map((d) => ({
      model: d.model.trim(),
      usd_per_mtok: {
        input: v(d.rates.input),
        output: v(d.rates.output),
        cache_read: v(d.rates.cache_read),
        cache_write: v(d.rates.cache_write),
        cache_write_5m: v(d.rates.cache_write),
        cache_write_1h: v(d.rates.cache_write_1h),
      },
    }));
    try {
      const next = await api.savePriceOverrides(overrides);
      qc.setQueryData(qk.pricing, next);
      void qc.invalidateQueries({ queryKey: ['usage'] });
      setEdits(null);
      toast({ tone: 'ok', title: 'Prices saved', message: 'They apply to new usage. Past requests keep the price they were recorded with.' });
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const providers = useMemo(() => [...new Set(t.rates.map((r) => r.provider))].sort(), [t.rates]);
  const rows = t.rates.filter((r) => (!provider || r.provider === provider) && (!query.trim() || r.model.toLowerCase().includes(query.trim().toLowerCase())));
  const scheduled = Array.isArray(t.scheduled) ? (t.scheduled as PriceRate[]).filter((r) => r && typeof r.model === 'string' && r.usd_per_mtok) : [];
  const unpriced = t.unpriced_models.filter((u) => !draft.some((d) => d.model === u.model));

  return (
    <div className="stack">
      <section className="card card-pad pricing-meta" aria-label="Price list">
        <div className="stack-sm" style={{ gap: 2 }}>
          <strong>Price list {t.version}</strong>
          <span className="muted small">Official list prices as of {fmtDate(t.as_of)}, in US dollars per million tokens.</span>
          {t.scope ? <span className="muted xs pricing-scope">{t.scope}</span> : null}
        </div>
        {t.sources.length ? (
          <ul className="price-sources">
            {t.sources.map((s) => (
              <li key={s.url}>
                <a className="link small" href={s.url} target="_blank" rel="noreferrer noopener">
                  {providerLabel(s.provider)} pricing <ExternalLink aria-hidden />
                </a>
                <span className="muted xs"> checked {fmtDate(s.retrieved)}</span>
              </li>
            ))}
          </ul>
        ) : null}
      </section>

      {unpriced.length ? (
        <Callout tone="warn" icon={TriangleAlert} title={`${unpriced.length} ${unpriced.length === 1 ? 'model has' : 'models have'} no price`}>
          <p className="small">Their usage shows as “not priced” instead of $0 until you set a price.</p>
          <ul className="unpriced-list">
            {unpriced.map((u) => (
              <li key={u.model}>
                <span className="mono">{u.model}</span>
                <span className="muted xs">
                  {formatNumber(u.units)} {u.units === 1 ? 'request' : 'requests'}
                  {u.provider ? ` · ${providerLabel(u.provider)}` : ''}
                  {u.last_seen_day || u.last_seen ? ` · last seen ${fmtDate(u.last_seen_day ?? u.last_seen)}` : ''}
                </span>
                <Button size="sm" icon={Plus} onClick={() => addRow(u.model)}>
                  Set price
                </Button>
              </li>
            ))}
          </ul>
        </Callout>
      ) : null}

      <section className="card" aria-labelledby="overrides-title">
        <div className="card-head">
          <h2 id="overrides-title">
            Your prices <span className="sub">for custom, local or discounted models</span>
          </h2>
          <Button size="sm" icon={Plus} onClick={() => addRow()} disabled={draft.length >= 200}>
            Add model
          </Button>
        </div>
        <div className="card-body stack">
          {draft.length ? (
            <div className="table-wrap">
              <table className="table override-table">
                <caption className="sr-only">Your price overrides, USD per million tokens</caption>
                <thead>
                  <tr>
                    <th scope="col">Model</th>
                    {RATE_COLS.map((c) => (
                      <th key={c.key} scope="col" title={c.hint}>
                        {c.label}
                      </th>
                    ))}
                    <th scope="col">
                      <span className="sr-only">Remove</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {draft.map((d) => (
                    <tr key={d.key}>
                      <td>
                        <input
                          id={`ovr-${d.key}-model`}
                          className="input mono"
                          value={d.model}
                          aria-label="Model"
                          aria-invalid={dupes.includes(d.model.trim()) || !d.model.trim() || undefined}
                          onChange={(e) => edit(draft.map((x) => (x.key === d.key ? { ...x, model: e.target.value } : x)))}
                          placeholder="model-id"
                          spellCheck={false}
                        />
                      </td>
                      {RATE_COLS.map((c) => {
                        const err = rateError(d.rates[c.key]);
                        return (
                          <td key={c.key}>
                            <input
                              id={`ovr-${d.key}-${c.key}`}
                              className="input num rate-input"
                              inputMode="decimal"
                              value={d.rates[c.key]}
                              aria-label={`${c.label} price for ${d.model || 'new model'}, USD per million tokens`}
                              aria-invalid={!!err || undefined}
                              title={err ?? undefined}
                              placeholder={c.key === 'input' || c.key === 'output' ? '0.00' : 'not set'}
                              onChange={(e) => edit(draft.map((x) => (x.key === d.key ? { ...x, rates: { ...x.rates, [c.key]: e.target.value } } : x)))}
                            />
                          </td>
                        );
                      })}
                      <td>
                        <Button size="sm" variant="ghost" iconOnly icon={X} aria-label={`Remove ${d.model || 'row'}`} onClick={() => edit(draft.filter((x) => x.key !== d.key))} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <p className="muted small">No custom prices. Add one for a local or private model so its usage gets an estimate.</p>
          )}
          {dirty && (problems.length || dupes.length || fieldErrors) ? (
            <Callout tone="err">
              <ul className="plain-list">
                {[...new Set(problems)].map((p) => (
                  <li key={p}>{p}</li>
                ))}
                {dupes.length ? <li>Each model can only have one price ({[...new Set(dupes)].join(', ')}).</li> : null}
                {fieldErrors ? <li>Prices are numbers like 1.25, up to 6 decimals and at most 10,000.</li> : null}
              </ul>
            </Callout>
          ) : null}
          {error ? (
            <Callout tone="err" title="Couldn’t save" role="alert">
              {error}
            </Callout>
          ) : null}
          <div className="row">
            <span className="muted xs">Applies to new usage only. Leave a cache price empty if the model has none; requests that use cache then stay unpriced.</span>
            <span className="spacer" />
            {dirty ? (
              <Button
                onClick={() => {
                  setEdits(null);
                  setError(null);
                }}
              >
                Discard
              </Button>
            ) : null}
            <Button variant="primary" onClick={() => void save()} disabled={!canSave} loading={saving}>
              Save prices
            </Button>
          </div>
        </div>
      </section>

      {scheduled.length ? (
        <section className="card" aria-labelledby="scheduled-title">
          <div className="card-head">
            <h2 id="scheduled-title">
              Upcoming price changes <span className="sub">applied automatically on the date shown</span>
            </h2>
          </div>
          <div className="table-wrap" tabIndex={0} role="region" aria-label="Upcoming price changes table">
            <table className="table rates-table">
              <caption className="sr-only">Scheduled prices in US dollars per million tokens</caption>
              <thead>
                <tr>
                  <th scope="col">Model</th>
                  {RATE_COLS.map((c) => (
                    <th key={c.key} scope="col" className="r" title={c.hint}>
                      {c.short}
                    </th>
                  ))}
                  <th scope="col">From</th>
                </tr>
              </thead>
              <tbody>
                {scheduled.map((r) => (
                  <tr key={`${r.provider}/${r.model}/${r.effective_from}`}>
                    <td>
                      <div className="usage-name">
                        <KindMark kind={providerKind(r.provider)} size="sm" />
                        <span className="mono">{r.model}</span>
                      </div>
                    </td>
                    {RATE_COLS.map((c) => (
                      <td key={c.key} className="r num">
                        {fmtRate(rateOf(r.usd_per_mtok, c.key)) ?? <span className="muted">–</span>}
                      </td>
                    ))}
                    <td>{r.effective_from ? fmtDate(r.effective_from) : <span className="muted">Not dated</span>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>
      ) : null}

      <section className="card" aria-labelledby="rates-title">
        <div className="card-head">
          <h2 id="rates-title">
            All prices <span className="sub">{t.rates.length} models</span>
          </h2>
          <div className="row">
            <div className="input-group">
              <Search className="input-icon" aria-hidden />
              <input className="input has-icon" type="search" placeholder="Find a model…" aria-label="Find a model" value={query} onChange={(e) => setQuery(e.target.value)} />
            </div>
            <select className="select" aria-label="Provider" value={provider} onChange={(e) => setProvider(e.target.value)} style={{ width: 'auto' }}>
              <option value="">All providers</option>
              {providers.map((p) => (
                <option key={p} value={p}>
                  {providerLabel(p)}
                </option>
              ))}
            </select>
          </div>
        </div>
        <div className="table-wrap" tabIndex={0} role="region" aria-label="All prices table">
          <table className="table rates-table">
            <caption className="sr-only">Prices in US dollars per million tokens</caption>
            <thead>
              <tr>
                <th scope="col">Model</th>
                {RATE_COLS.map((c) => (
                  <th key={c.key} scope="col" className="r" title={c.hint}>
                    {c.short}
                  </th>
                ))}
                <th scope="col">Source</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={`${r.provider}/${r.model}/${r.origin}`}>
                  <td>
                    <div className="usage-name">
                      <KindMark kind={providerKind(r.provider)} size="sm" />
                      <span className="mono">{r.model}</span>
                    </div>
                    {r.long_context ? (
                      <div className="muted xs">
                        Above {formatNumber(r.long_context.above_input_tokens)} input tokens: {fmtRate(r.long_context.usd_per_mtok.input) ?? '?'} in, {fmtRate(r.long_context.usd_per_mtok.output) ?? '?'} out
                      </div>
                    ) : null}
                    {r.note ? <div className="muted xs">{r.note}</div> : null}
                    {r.effective_until ? <div className="muted xs">Until {fmtDate(r.effective_until)}</div> : null}
                  </td>
                  {RATE_COLS.map((c) => (
                    <td key={c.key} className="r num">
                      {fmtRate(rateOf(r.usd_per_mtok, c.key)) ?? <span className="muted">–</span>}
                    </td>
                  ))}
                  <td>{r.origin === 'override' ? <Badge tone="brand">Your price</Badge> : <Badge tone="outline">Official</Badge>}</td>
                </tr>
              ))}
              {!rows.length ? (
                <tr>
                  <td colSpan={7} className="muted small">
                    No models match.
                  </td>
                </tr>
              ) : null}
            </tbody>
          </table>
        </div>
      </section>
    </div>
  );
}
