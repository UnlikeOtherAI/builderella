import { useQueryClient } from '@tanstack/react-query';
import { useState, type FormEvent } from 'react';
import { ApiFailure, api, describeError, setToken } from '../api/client';

export function SignIn() {
  const queryClient = useQueryClient();
  const [token, setValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<{ title: string; detail: string } | null>(null);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const candidate = token.trim();
    if (candidate === '') return;
    setBusy(true);
    setProblem(null);
    try {
      // Check the token before keeping it.
      const who = await api.whoami(undefined, candidate);
      queryClient.setQueryData(['whoami'], who);
      setToken(candidate);
    } catch (error) {
      setProblem(
        error instanceof ApiFailure && error.status === 401
          ? {
              title: 'The controller did not accept that token',
              detail: 'Check that it was copied whole, and that it has not been revoked.',
            }
          : describeError(error),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="signin">
      <h1>Builderella</h1>
      <form onSubmit={(event) => void submit(event)} className="card form">
        <h2>Sign in</h2>
        <div className="field">
          <label htmlFor="token">API token</label>
          <input
            id="token"
            type="password"
            value={token}
            onChange={(event) => setValue(event.target.value)}
            autoComplete="off"
            spellCheck={false}
            aria-describedby="token-hint"
            autoFocus
          />
          <p className="hint" id="token-hint">
            An administrator creates tokens under Administration or with the CLI. The token is kept in
            this browser's local storage until you sign out or the controller rejects it.
          </p>
        </div>
        {problem ? (
          <div className="state state-error" role="alert">
            <p className="state-title">{problem.title}</p>
            <p className="state-detail pre">{problem.detail}</p>
          </div>
        ) : null}
        <div className="actions actions-start">
          <button type="submit" className="btn btn-primary" disabled={busy || token.trim() === ''}>
            {busy ? 'Checking…' : 'Sign in'}
          </button>
        </div>
      </form>
    </main>
  );
}
