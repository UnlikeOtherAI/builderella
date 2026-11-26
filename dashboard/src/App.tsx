import { useQueryClient } from '@tanstack/react-query';
import { useEffect } from 'react';
import { HashRouter, Navigate, Route, Routes, useParams } from 'react-router';
import { isMock } from './api/client';
import { useToken } from './api/hooks';
import { LiveProvider } from './api/live';
import { Shell } from './components/Shell';
import { JobDetailView } from './views/JobDetail';
import { JobsView } from './views/Jobs';
import { MachineDetailView } from './views/MachineDetail';
import { MachinesView } from './views/Machines';
import { OverviewView } from './views/Overview';
import { QueueView } from './views/Queue';
import { ServiceDetailView, ServicesView } from './views/Services';
import { SettingsView } from './views/Settings';
import { SignIn } from './views/SignIn';

export function App() {
  const token = useToken();
  const queryClient = useQueryClient();
  // Signed out, or the controller rejected the token: keep nothing that was
  // read with it.
  useEffect(() => {
    if (!isMock && !token) queryClient.clear();
  }, [token, queryClient]);
  if (!isMock && !token) return <SignIn />;
  return (
    // A different token is a different session: start its stream afresh.
    <LiveProvider key={token ?? 'mock'} token={token}>
      <HashRouter>
        <Shell signedIn={token !== null}>
          <AppRoutes />
        </Shell>
      </HashRouter>
    </LiveProvider>
  );
}

/** An old route with a parameter, sent to its new name. */
function Renamed({ to }: { to: string }) {
  const params = useParams();
  const id = params.id ?? params.machine;
  return <Navigate to={id ? `${to}/${encodeURIComponent(id)}` : to} replace />;
}

/**
 * Every page. `/jobs/<id>` is printed by the CLI and the MCP server, so it
 * never moves; the routes of earlier versions redirect to their new names.
 */
export function AppRoutes() {
  return (
    <Routes>
      <Route path="/" element={<OverviewView />} />
      <Route path="/machines" element={<MachinesView />} />
      <Route path="/machines/:id" element={<MachineDetailView />} />
      <Route path="/jobs" element={<JobsView />} />
      <Route path="/jobs/:id" element={<JobDetailView />} />
      <Route path="/queue" element={<QueueView />} />
      <Route path="/services" element={<ServicesView />} />
      <Route path="/services/:id" element={<ServiceDetailView />} />
      <Route path="/settings" element={<SettingsView />} />
      {/* Earlier names. */}
      <Route path="/fleet" element={<Renamed to="/machines" />} />
      <Route path="/fleet/:machine" element={<Renamed to="/machines" />} />
      <Route path="/environments" element={<Renamed to="/services" />} />
      <Route path="/environments/:id" element={<Renamed to="/services" />} />
      <Route path="/admin" element={<Renamed to="/settings" />} />
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
