import { useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { HashRouter, Navigate, NavLink, Route, Routes } from 'react-router';
import { clearToken, isMock } from './api/client';
import { useToken, useWhoAmI } from './api/hooks';
import { FALLBACK_POLL_MS, LiveProvider, useLive } from './api/live';
import { ErrorPanel } from './components/ui';
import { roleLabel } from './lib/labels';
import { applyTheme, nextTheme, readTheme, type ThemeChoice } from './lib/theme';
import { AdminView } from './views/Admin';
import { EnvironmentsView } from './views/Environments';
import { FleetView } from './views/Fleet';
import { JobView } from './views/Job';
import { QueueView } from './views/Queue';
import { SignIn } from './views/SignIn';

export function App() {
  const token = useToken();
  if (!isMock && !token) return <SignIn />;
  return (
    // A different token is a different session: start its stream afresh.
    <LiveProvider key={token ?? 'mock'} token={token}>
      <HashRouter>
        <Shell signedIn={token !== null} />
      </HashRouter>
    </LiveProvider>
  );
}

const THEME_LABEL: Record<ThemeChoice, string> = {
  system: 'Theme: system',
  light: 'Theme: light',
  dark: 'Theme: dark',
};

function LiveStatus() {
  const live = useLive();
  const seconds = FALLBACK_POLL_MS / 1000;
  const text =
    live.status === 'open'
      ? 'Live updates on'
      : live.status === 'connecting'
        ? 'Connecting to live updates'
        : live.status === 'reconnecting'
          ? `Live updates lost; refreshing every ${seconds} s`
          : `Live updates off; refreshing every ${seconds} s`;
  return (
    <span className={`live live-${live.status === 'open' ? 'on' : 'off'}`} role="status" title={live.detail}>
      {text}
    </span>
  );
}

function Shell({ signedIn }: { signedIn: boolean }) {
  const queryClient = useQueryClient();
  const who = useWhoAmI();
  const live = useLive();
  const [theme, setTheme] = useState<ThemeChoice>(readTheme);

  const signOut = () => {
    clearToken();
    queryClient.clear();
  };
  const link = ({ isActive }: { isActive: boolean }) => (isActive ? 'nav-link active' : 'nav-link');

  return (
    <>
      <a className="skip" href="#main">
        Skip to content
      </a>
      <header className="top">
        <span className="brand">Builderella</span>
        <nav aria-label="Views" className="nav">
          <NavLink to="/" end className={link}>
            Fleet
          </NavLink>
          <NavLink to="/queue" className={link}>
            Queue
          </NavLink>
          <NavLink to="/jobs" className={link}>
            Job
          </NavLink>
          <NavLink to="/environments" className={link}>
            Environments
          </NavLink>
          <NavLink to="/admin" className={link}>
            Administration
          </NavLink>
        </nav>
        <div className="tools">
          {isMock ? (
            <span className="mock-badge" title="These are sample values, not a real controller.">
              Mock data
            </span>
          ) : null}
          <LiveStatus />
          {who.data ? (
            <span className="who">
              {who.data.name} ({roleLabel(who.data.role)})
            </span>
          ) : null}
          <button
            type="button"
            className="btn btn-small"
            onClick={() => {
              const next = nextTheme(theme);
              applyTheme(next);
              setTheme(next);
            }}
          >
            {THEME_LABEL[theme]}
          </button>
          {signedIn ? (
            <button type="button" className="btn btn-small" onClick={signOut}>
              Sign out
            </button>
          ) : null}
        </div>
      </header>

      {live.drift ? (
        <div className="state state-error banner" role="alert">
          <p className="state-title">An update from the controller was not understood</p>
          <p className="state-detail">
            An event did not match the expected shape and was skipped, so live updates may miss
            changes. Every view now refreshes on a timer instead. The dashboard and the controller are
            probably different versions.
          </p>
          <details>
            <summary>What did not match</summary>
            <p className="pre">{live.drift}</p>
          </details>
        </div>
      ) : null}

      <main id="main" tabIndex={-1}>
        {who.error && !who.data ? (
          <ErrorPanel
            error={who.error}
            onRetry={() => void who.refetch()}
            context="Your role could not be read, so actions that need a role may be refused."
          />
        ) : null}
        <Routes>
          <Route path="/" element={<FleetView />} />
          <Route path="/queue" element={<QueueView />} />
          <Route path="/jobs" element={<JobView />} />
          <Route path="/jobs/:id" element={<JobView />} />
          <Route path="/environments" element={<EnvironmentsView />} />
          <Route path="/environments/:id" element={<EnvironmentsView />} />
          <Route path="/admin" element={<AdminView />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </>
  );
}
