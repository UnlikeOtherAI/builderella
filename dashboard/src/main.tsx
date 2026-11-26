import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { NetworkError, isMock, setTransport } from './api/client';
import { setEventConnect } from './api/live';
import { applyTheme, readTheme } from './lib/theme';
import './styles.css';

applyTheme(readTheme());

if (isMock) {
  // Loaded only in mock mode, so the fixtures stay out of the normal bundle.
  const [{ createMock }, fixtures] = await Promise.all([
    import('./api/mock'),
    import('./mocks/fixtures.json'),
  ]);
  const role = new URLSearchParams(window.location.search).get('role') ?? undefined;
  const mock = createMock(fixtures.default, { role });
  setTransport(mock.transport);
  setEventConnect(mock.connect);
}

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 2000,
      // Only a failure to reach the controller is worth repeating; an error
      // answer or an unexpected shape will not change by asking again.
      retry: (failures, error) => error instanceof NetworkError && failures < 2,
    },
    mutations: { retry: false },
  },
});

const root = document.getElementById('root');
if (!root) throw new Error('index.html has no #root element');
createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
