import { LayoutDashboard, SignpostBig } from 'lucide-react';
import { navigate } from '../app/router';
import { Button, EmptyState, PageHead } from '../components/ui';

export function NotFoundPage() {
  return (
    <>
      <PageHead title="Off the rails" />
      <div className="card">
        <EmptyState
          icon={SignpostBig}
          title="There’s no page at this address"
          actions={
            <Button variant="primary" icon={LayoutDashboard} onClick={() => navigate('/')}>
              Back to overview
            </Button>
          }
        >
          The link may be outdated. Everything in Switchyard is reachable from the sidebar.
        </EmptyState>
      </div>
    </>
  );
}
