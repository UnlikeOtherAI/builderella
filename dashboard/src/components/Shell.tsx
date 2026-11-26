// The frame around every page: a sidebar with the sections (collapsible to
// icons, a menu on narrow screens) and a top bar with the fleet, the live
// connection, who is signed in, the theme and sign-out.

import { useEffect, useState, type ReactNode } from 'react';
import { NavLink, useLocation } from 'react-router';
import { clearToken, isMock } from '../api/client';
import { useFleet, useWhoAmI } from '../api/hooks';
import { FALLBACK_POLL_MS, useLive } from '../api/live';
import { shortId } from '../lib/format';
import { roleLabel } from '../lib/labels';
import { isOnline, machineStatus } from '../lib/status';
import { applyTheme, nextTheme, readTheme, type ThemeChoice } from '../lib/theme';
import { Icon, type IconName } from './icons';
import { ErrorPanel } from './ui';

const SIDEBAR_KEY = 'bella.sidebar';

function readCollapsed(): boolean {
  try {
    return window.localStorage.getItem(SIDEBAR_KEY) === 'collapsed';
  } catch {
    return false;
  }
}

function storeCollapsed(collapsed: boolean): void {
  try {
    if (collapsed) window.localStorage.setItem(SIDEBAR_KEY, 'collapsed');
    else window.localStorage.removeItem(SIDEBAR_KEY);
  } catch {
    // Applies to this page only.
  }
}

interface NavItem {
  to: string;
  label: string;
  icon: IconName;
  end?: boolean;
}

export const NAV: NavItem[] = [
  { to: '/', label: 'Overview', icon: 'overview', end: true },
  { to: '/machines', label: 'Machines', icon: 'machines' },
  { to: '/jobs', label: 'Jobs', icon: 'jobs' },
  { to: '/queue', label: 'Queue', icon: 'queue' },
  { to: '/services', label: 'Services', icon: 'services' },
  { to: '/settings', label: 'Settings', icon: 'settings' },
];

function useNavBadges() {
  const fleet = useFleet();
  if (!fleet.data) return { machines: null, queue: null };
  const trouble = fleet.data.machines.filter((machine) => {
    const status = machineStatus(machine);
    return !isOnline(status) && status.key !== 'retired';
  }).length;
  return { machines: trouble > 0 ? trouble : null, queue: fleet.data.queued > 0 ? fleet.data.queued : null };
}

function Sidebar({ collapsed, onToggle }: { collapsed: boolean; onToggle: () => void }) {
  const badges = useNavBadges();
  return (
    <>
      <div className="brand">
        <span className="brand-mark" aria-hidden="true">
          B
        </span>
        <span className="brand-name">Builderella</span>
      </div>
      <nav aria-label="Sections" className="nav">
        <ul>
          {NAV.map((item) => {
            const badge =
              item.to === '/machines' ? badges.machines : item.to === '/queue' ? badges.queue : null;
            const badgeText =
              item.to === '/machines' ? `${badge} need attention` : `${badge} waiting`;
            return (
              <li key={item.to}>
                <NavLink
                  to={item.to}
                  end={item.end}
                  className={({ isActive }) => (isActive ? 'nav-link active' : 'nav-link')}
                  title={collapsed ? item.label : undefined}
                >
                  <Icon name={item.icon} size={19} />
                  <span className="nav-label">{item.label}</span>
                  {badge ? (
                    <span
                      className={item.to === '/machines' ? 'nav-badge nav-badge-warn' : 'nav-badge'}
                      title={badgeText}
                    >
                      {badge}
                      <span className="sr-only"> {item.to === '/machines' ? 'need attention' : 'waiting'}</span>
                    </span>
                  ) : null}
                </NavLink>
              </li>
            );
          })}
        </ul>
      </nav>
      <button
        type="button"
        className="nav-collapse"
        onClick={onToggle}
        aria-label={collapsed ? 'Expand the sidebar' : 'Collapse the sidebar'}
        title={collapsed ? 'Expand the sidebar' : 'Collapse the sidebar'}
      >
        <Icon name={collapsed ? 'chevronRight' : 'chevronLeft'} />
        <span className="nav-label">Collapse</span>
      </button>
    </>
  );
}

function LiveIndicator() {
  const live = useLive();
  const seconds = FALLBACK_POLL_MS / 1000;
  const [label, tone, detail] =
    live.status === 'open' && !live.drift
      ? ['Live', 'ok', 'Updates arrive as they happen.']
      : live.status === 'open'
        ? ['Live, partly', 'warn', `An update was not understood; pages also refresh every ${seconds} s.`]
        : live.status === 'connecting'
          ? ['Connecting', 'neutral', 'Connecting to live updates.']
          : ['Not live', 'warn', `Live updates are down; pages refresh every ${seconds} s.`];
  return (
    <span className={`live status status-${tone}`} role="status" title={live.detail ? `${detail} ${live.detail}` : detail}>
      <span className={tone === 'ok' ? 'dot dot-live' : 'dot'} aria-hidden="true" />
      <span className="live-label">{label}</span>
      <span className="sr-only">. {detail}</span>
    </span>
  );
}

const THEME_ICON: Record<ThemeChoice, IconName> = { system: 'system', light: 'sun', dark: 'moon' };

function FleetName() {
  const fleet = useFleet();
  if (!fleet.data) return <span className="fleet-name muted">Fleet</span>;
  return (
    <span className="fleet-name" title={`Fleet ${fleet.data.fleet_id}, controller ${fleet.data.controller_version}`}>
      <strong>Fleet {shortId(fleet.data.fleet_id)}</strong>
      <span className="muted fleet-version"> controller {fleet.data.controller_version}</span>
    </span>
  );
}

export function Shell({ signedIn, children }: { signedIn: boolean; children: ReactNode }) {
  const who = useWhoAmI();
  const live = useLive();
  const location = useLocation();
  const [theme, setTheme] = useState<ThemeChoice>(readTheme);
  const [collapsed, setCollapsed] = useState(readCollapsed);
  const [menuOpen, setMenuOpen] = useState(false);

  // A chosen page closes the menu on a narrow screen.
  useEffect(() => setMenuOpen(false), [location.pathname]);
  useEffect(() => {
    if (!menuOpen) return;
    const close = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') setMenuOpen(false);
    };
    document.addEventListener('keydown', close);
    return () => document.removeEventListener('keydown', close);
  }, [menuOpen]);

  const next = nextTheme(theme);
  return (
    <div className={`app${collapsed ? ' app-collapsed' : ''}${menuOpen ? ' app-menu-open' : ''}`}>
      <a className="skip" href="#main">
        Skip to content
      </a>
      <aside className="sidebar" id="sidebar" aria-label="Navigation">
        <Sidebar
          collapsed={collapsed}
          onToggle={() => {
            storeCollapsed(!collapsed);
            setCollapsed(!collapsed);
          }}
        />
      </aside>
      {menuOpen ? <div className="scrim" aria-hidden="true" onClick={() => setMenuOpen(false)} /> : null}

      <div className="main-col">
        <header className="topbar">
          <button
            type="button"
            className="icon-btn menu-btn"
            aria-label={menuOpen ? 'Close the menu' : 'Open the menu'}
            aria-expanded={menuOpen}
            aria-controls="sidebar"
            onClick={() => setMenuOpen(!menuOpen)}
          >
            <Icon name={menuOpen ? 'close' : 'menu'} />
          </button>
          <FleetName />
          <div className="topbar-tools">
            {isMock ? (
              <span className="mock-badge" title="These are sample values, not a real controller.">
                Mock data
              </span>
            ) : null}
            <LiveIndicator />
            {who.data ? (
              <span className="who" title={`Signed in as ${who.data.name}, ${roleLabel(who.data.role)}`}>
                <span className="who-name">{who.data.name}</span>
                <span className="role">{roleLabel(who.data.role)}</span>
              </span>
            ) : null}
            <button
              type="button"
              className="icon-btn"
              aria-label={`Theme: ${theme}. Switch to ${next}.`}
              title={`Theme: ${theme}. Switch to ${next}.`}
              onClick={() => {
                applyTheme(next);
                setTheme(next);
              }}
            >
              <Icon name={THEME_ICON[theme]} />
            </button>
            {signedIn ? (
              <button type="button" className="icon-btn" aria-label="Sign out" title="Sign out" onClick={() => clearToken()}>
                <Icon name="signOut" />
              </button>
            ) : null}
          </div>
        </header>

        <main id="main" tabIndex={-1} className="content">
          {live.drift ? (
            <div className="callout callout-warn banner" role="alert">
              <Icon name="alert" />
              <div>
                <p className="callout-title">An update from the controller was not understood</p>
                <p className="callout-body">
                  It was skipped, so live updates may miss changes; every page now also refreshes on a timer.
                  The dashboard and the controller are probably different versions.
                </p>
                <details>
                  <summary>What did not match</summary>
                  <p className="pre">{live.drift}</p>
                </details>
              </div>
            </div>
          ) : null}
          {who.error && !who.data ? (
            <ErrorPanel
              error={who.error}
              onRetry={() => void who.refetch()}
              context="Your role could not be read, so actions that need a role may be refused."
            />
          ) : null}
          {children}
        </main>
      </div>
    </div>
  );
}
