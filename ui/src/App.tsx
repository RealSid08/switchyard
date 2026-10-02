import { QueryClientProvider } from '@tanstack/react-query';
import { lazy, Suspense, useState } from 'react';
import { AuthProvider } from './app/auth';
import { LiveProvider } from './app/live';
import { createQueryClient } from './app/queries';
import { matchPath, RouterProvider, useLocation } from './app/router';
import { Shell } from './app/Shell';
import { ConfirmProvider, ToastProvider } from './components/feedback';
import { ActivityPage } from './pages/Activity';
import { ClientsPage } from './pages/Clients';
import { ConnectionsPage } from './pages/Connections';
import { KeysPage } from './pages/Keys';
import { NotFoundPage } from './pages/NotFound';
import { OverviewPage } from './pages/Overview';
import { PlaygroundPage } from './pages/Playground';
import { RoutesPage } from './pages/Routes';
import { SettingsPage } from './pages/Settings';
import { UsagePage } from './pages/Usage';

// Dev-only mock controls. `import.meta.env.VITE_SWITCHYARD_MOCK` is undefined in
// production builds, so this branch (and the module) is dropped entirely.
const MockPanel = import.meta.env.VITE_SWITCHYARD_MOCK ? lazy(() => import('./dev/MockPanel')) : null;

function Page() {
  const { path } = useLocation();
  if (path === '/') return <OverviewPage />;
  if (path === '/connections') return <ConnectionsPage />;
  if (path === '/routes') return <RoutesPage />;
  if (path === '/activity') return <ActivityPage />;
  const detail = matchPath('/activity/:id', path);
  if (detail) return <ActivityPage selectedId={detail.id} />;
  if (path === '/usage' || path === '/usage/limits' || path === '/usage/pricing') return <UsagePage />;
  if (path === '/playground') return <PlaygroundPage />;
  if (path === '/clients') return <ClientsPage />;
  if (path === '/keys') return <KeysPage />;
  if (path === '/settings') return <SettingsPage />;
  return <NotFoundPage />;
}

export function App() {
  const [queryClient] = useState(createQueryClient);
  return (
    <QueryClientProvider client={queryClient}>
      <ToastProvider>
        <ConfirmProvider>
          <RouterProvider>
            <AuthProvider>
              {(ready) => (
                <LiveProvider enabled={ready}>
                  <Shell>
                    <Page />
                  </Shell>
                </LiveProvider>
              )}
            </AuthProvider>
            {MockPanel ? (
              <Suspense fallback={null}>
                <MockPanel />
              </Suspense>
            ) : null}
          </RouterProvider>
        </ConfirmProvider>
      </ToastProvider>
    </QueryClientProvider>
  );
}
