import { type FormEvent, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';

import { DESKTOP_PRESETS } from '../rdp';
import { saveConnectRequest } from '../session-store';

export function ConnectPage() {
  const navigate = useNavigate();
  const [hostname, setHostname] = useState('103.173.99.103:3389');
  const [username, setUsername] = useState('Administrator');
  const [password, setPassword] = useState('hP4M95R5jMuP273');
  const [domain, setDomain] = useState('');
  const [desktop, setDesktop] = useState(`${DESKTOP_PRESETS[0].width}x${DESKTOP_PRESETS[0].height}`);
  const [error, setError] = useState('');

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    const host = hostname.trim();
    const user = username.trim();
    const port = host.includes(':') ? host.slice(host.lastIndexOf(':') + 1) : '';
    if (!/^\d+$/.test(port)) {
      setError('Hostname must be host:port, for example 103.173.99.103:3389.');
      return;
    }
    if (user === '' || password === '') {
      setError('Username and password are required.');
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
          Hostname
          <input
            value={hostname}
            onChange={(event) => setHostname(event.target.value)}
            placeholder="103.173.99.103:3389"
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
        <label>
          Domain
          <span className="optional">optional</span>
          <input
            value={domain}
            onChange={(event) => setDomain(event.target.value)}
            autoComplete="off"
          />
        </label>

        {error !== '' ? <p className="form-error">{error}</p> : null}

        <button type="submit">Connect</button>
      </form>
    </main>
  );
}
