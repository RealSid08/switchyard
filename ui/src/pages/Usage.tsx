import { Coins, Gauge, Tags } from 'lucide-react';
import { Link, useLocation } from '../app/router';
import { PageHead } from '../components/ui';
import { LimitsTab } from './usage/Limits';
import { PricingTab } from './usage/Pricing';
import { SpendTab } from './usage/Spend';

const TABS = [
  { to: '/usage', label: 'Spend & tokens', icon: Coins },
  { to: '/usage/limits', label: 'Plan limits', icon: Gauge },
  { to: '/usage/pricing', label: 'Pricing', icon: Tags },
] as const;

const DESCRIPTIONS: Record<string, string> = {
  '/usage': 'What your traffic used and what it would cost, by account, model and app.',
  '/usage/limits': 'How close each account is to its plan limits, with balances and billing the providers report.',
  '/usage/pricing': 'The list prices behind every estimate, where they come from, and your own prices for models without one.',
};

export function UsagePage() {
  const { path, search } = useLocation();
  const current = TABS.find((t) => t.to === path) ?? TABS[0];
  // Keep window/filters when hopping between tabs.
  const keep = (to: string) => (to === '/usage' || to === '/usage/limits' ? `${to}${search}` : to);
  return (
    <>
      <PageHead title="Usage" description={DESCRIPTIONS[current.to]} />
      <nav className="subnav" aria-label="Usage sections">
        {TABS.map((t) => (
          <Link key={t.to} to={keep(t.to)} aria-current={t.to === current.to ? 'page' : undefined}>
            <t.icon aria-hidden />
            {t.label}
          </Link>
        ))}
      </nav>
      {current.to === '/usage' ? <SpendTab /> : current.to === '/usage/limits' ? <LimitsTab /> : <PricingTab />}
    </>
  );
}
