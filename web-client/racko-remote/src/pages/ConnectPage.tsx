import { type FormEvent, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';

import { modeBDesktopSize, saveModeBRequest } from '../modeb';
import { DESKTOP_PRESETS } from '../rdp';
import { saveConnectRequest } from '../session-store';

type SessionMode = 'mode-c' | 'mode-b';

export function ConnectPage() {
  const navigate = useNavigate();
  const [mode, setMode] = useState<SessionMode>('mode-c');
  const [hostname, setHostname] = useState(import.meta.env.VITE_DEV_RDP_HOST ?? '');
  const [username, setUsername] = useState(import.meta.env.VITE_DEV_RDP_USER ?? '');
  const [password, setPassword] = useState('');
  const [domain, setDomain] = useState('');
  const [desktop, setDesktop] = useState(`${DESKTOP_PRESETS[0].width}x${DESKTOP_PRESETS[0].height}`);
  const [error, setError] = useState('');

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    const host = hostname.trim();
    const user = username.trim();
    const port = host.includes(':') ? host.slice(host.lastIndexOf(':') + 1) : '';
    if (!/^\d+$/.test(port)) {
      setError('Hostname must be host:port, for example rdp.example.com:3389.');
      return;
    }
    if (user === '' || password === '') {
      setError('Username and password are required.');
      return;
    }
    if (mode === 'mode-b') {
      saveModeBRequest({ hostname: host, username: user, password, domain: domain.trim(), ...modeBDesktopSize() });
      navigate('/modeb');
      return;
    }
    const preset = DESKTOP_PRESETS.find((size) => `${size.width}x${size.height}` === desktop);
    if (preset == null) {
      setError('Choose a desktop resolution.');
      return;
    }
    saveConnectRequest({
      hostname: host,
      username: user,
      password,
      domain: domain.trim(),
      desktopWidth: preset.width,
      desktopHeight: preset.height,
    });
    navigate('/session');
  }

  return (
    <main className="connect">
      <form className="card" onSubmit={onSubmit}>
        <p className="mark">Racko</p>
        <h1>Remote desktop</h1>
        <p className="lede">Connect through the local RDCleanPath broker.</p>
        <nav className="app-nav">
          <Link to="/" aria-current="page">
            Connect
          </Link>
          <Link to="/dashboard">Dashboard</Link>
        </nav>

        <label>
          Mode
          <select value={mode} onChange={(event) => setMode(event.target.value as SessionMode)}>
            <option value="mode-c">Mode C (browser decode)</option>
            <option value="mode-b">Mode B (GPU stream)</option>
          </select>
        </label>
        <label>
          Hostname
          <input
            value={hostname}
            onChange={(event) => setHostname(event.target.value)}
            placeholder="rdp.example.com:3389"
            autoComplete="off"
            spellCheck={false}
          />
        </label>
        <label>
          Username
          <input
            value={username}
            onChange={(event) => setUsername(event.target.value)}
            autoComplete="username"
          />
        </label>
        <label>
          Password
          <input
            type="password"
            value={password}
            onChange={(event) => setPassword(event.target.value)}
            autoComplete="current-password"
          />
        </label>
        {mode === 'mode-c' ? (
          <label>
            Resolution
            <select value={desktop} onChange={(event) => setDesktop(event.target.value)}>
              {DESKTOP_PRESETS.map((size) => (
                <option key={`${size.width}x${size.height}`} value={`${size.width}x${size.height}`}>
                  {size.width}×{size.height}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        <label>
          Domain
          <span className="optional">optional</span>
          <input
            value={domain}
            onChange={(event) => setDomain(event.target.value)}
            autoComplete="off"
          />
        </label>
        {mode === 'mode-b' ? (
          <p className="lede">The broker signs in with these credentials; the desktop matches your screen (up to 1920×1200).</p>
        ) : null}

        {error !== '' ? <p className="form-error">{error}</p> : null}

        <button type="submit">Connect</button>
      </form>
    </main>
  );
}
