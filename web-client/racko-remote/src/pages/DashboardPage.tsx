import { useCallback, useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';

import { brokerUrl, type BrokerSession } from '../broker';

const POLL_MS = 2000;

export function DashboardPage() {
  const [sessions, setSessions] = useState<BrokerSession[]>([]);
  const [error, setError] = useState('');
  const [throughput, setThroughput] = useState<number | null>(null);
  const previous = useRef<{ at: number; bytes: number } | null>(null);

  const load = useCallback(async () => {
    try {
      const response = await fetch(brokerUrl('/api/sessions'));
      if (!response.ok) {
        throw new Error(`Broker returned ${response.status}`);
      }
      const next = (await response.json()) as BrokerSession[];
      const bytes = next.reduce((sum, session) => sum + session.bytesSent + session.bytesReceived, 0);
      const now = Date.now();
      const prior = previous.current;
      if (prior != null) {
        const elapsed = (now - prior.at) / 1000;
        if (elapsed > 0) {
          setThroughput(Math.max(0, (bytes - prior.bytes) / elapsed));
        }
      }
      previous.current = { at: now, bytes };
      setSessions(next);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not reach the broker');
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => {
      void load();
    }, POLL_MS);
    return () => window.clearInterval(timer);
  }, [load]);

  async function kill(session: BrokerSession) {
    const confirmed = window.confirm(`Kill the session to ${session.dest}?`);
    if (!confirmed) {
      return;
    }
    try {
      const response = await fetch(brokerUrl(`/api/sessions/${session.id}/kill`), {
        method: 'POST',
      });
      if (!response.ok) {
        throw new Error(response.status === 404 ? 'Session already ended' : `Kill failed (${response.status})`);
      }
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Kill failed');
    }
    await load();
  }

  const sent = sessions.reduce((sum, session) => sum + session.bytesSent, 0);
  const received = sessions.reduce((sum, session) => sum + session.bytesReceived, 0);

  return (
    <main className="dashboard">
      <header className="dash-head">
        <div>
          <p className="mark">Racko</p>
          <h1>Sessions</h1>
        </div>
        <nav className="app-nav">
          <Link to="/">Connect</Link>
          <Link to="/dashboard" aria-current="page">
            Dashboard
          </Link>
        </nav>
      </header>

      <section className="dash-stats">
        <p>
          <span>Active sessions</span>
          <strong>{sessions.length}</strong>
        </p>
        <p>
          <span>Combined throughput</span>
          <strong>{throughput == null ? 'measuring…' : `${formatBytes(throughput)}/s`}</strong>
        </p>
        <p>
          <span>Sent / received</span>
          <strong>
            {formatBytes(sent)} / {formatBytes(received)}
          </strong>
        </p>
      </section>

      {error !== '' ? <p className="form-error">{error}</p> : null}

      {sessions.length === 0 ? (
        <p className="empty">No active sessions. Connect from the login page to see one here.</p>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Mode</th>
                <th>Destination</th>
                <th>Client IP</th>
                <th>Duration</th>
                <th>Bytes sent</th>
                <th>Bytes received</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {sessions.map((session) => (
                <tr key={session.id}>
                  <td title={session.mode === 'B' ? 'GPU stream' : 'Browser decode'}>{session.mode}</td>
                  <td>{session.dest}</td>
                  <td>{session.clientIp}</td>
                  <td>{formatDuration(session.durationSecs)}</td>
                  <td>{formatBytes(session.bytesSent)}</td>
                  <td>{formatBytes(session.bytesReceived)}</td>
                  <td>
                    <button type="button" className="danger" onClick={() => void kill(session)}>
                      Kill
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </main>
  );
}

function formatDuration(durationSecs: number): string {
  const total = Math.max(0, Math.floor(durationSecs));
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`;
}

function formatBytes(value: number): string {
  const n = Math.max(0, value);
  if (n < 1024) {
    return `${Math.round(n)} B`;
  }
  if (n < 1024 * 1024) {
    return `${(n / 1024).toFixed(1)} KB`;
  }
  if (n < 1024 * 1024 * 1024) {
    return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  }
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}
