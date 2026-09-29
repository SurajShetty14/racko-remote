import { useEffect, useRef, useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';

import {
  AUTH_FAILED,
  INITIAL_MODEB_STATE,
  clearModeBRequest,
  openModeBSession,
  readModeBRequest,
  type InputMessage,
  type ModeBSessionHandle,
  type ModeBState,
} from '../modeb';

type LinkState = 'connecting' | 'connected' | 'lost' | 'error';

const CTRL_ALT_DEL: readonly InputMessage[] = [
  { type: 'keydown', code: 'ControlLeft', repeat: false },
  { type: 'keydown', code: 'AltLeft', repeat: false },
  { type: 'keydown', code: 'Delete', repeat: false },
  { type: 'keyup', code: 'Delete' },
  { type: 'keyup', code: 'AltLeft' },
  { type: 'keyup', code: 'ControlLeft' },
];

export function ModeBSession() {
  const navigate = useNavigate();
  const [request] = useState(readModeBRequest);
  const hostname = request?.hostname ?? null;
  const screenRef = useRef<HTMLDivElement>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const sessionRef = useRef<ModeBSessionHandle | null>(null);
  const [state, setState] = useState<ModeBState>(INITIAL_MODEB_STATE);
  const [fit, setFit] = useState(true);
  const [fullscreen, setFullscreen] = useState(false);
  const [barPeek, setBarPeek] = useState(false);

  useEffect(() => {
    const video = videoRef.current;
    if (request == null || video == null) {
      return;
    }
    setState(INITIAL_MODEB_STATE);
    let session: ModeBSessionHandle | null = null;
    // StrictMode mounts effects twice in dev, and the signaling server serves a single socket per
    // session, so only open it once the mount has settled.
    const timer = window.setTimeout(() => {
      session = openModeBSession(video, request, setState);
      sessionRef.current = session;
    }, 0);
    return () => {
      window.clearTimeout(timer);
      session?.close();
      sessionRef.current = null;
    };
  }, [request]);

  useEffect(() => {
    const screen = screenRef.current;

    function onFullscreenChange() {
      setFullscreen(screen != null && document.fullscreenElement === screen);
      setBarPeek(false);
    }

    document.addEventListener('fullscreenchange', onFullscreenChange);
    return () => document.removeEventListener('fullscreenchange', onFullscreenChange);
  }, []);

  async function toggleFullscreen() {
    const screen = screenRef.current;
    if (screen == null) {
      return;
    }
    try {
      if (document.fullscreenElement === screen) {
        await document.exitFullscreen();
        return;
      }
      await screen.requestFullscreen();
      videoRef.current?.focus();
    } catch (error) {
      console.error('Fullscreen was blocked', error);
    }
  }

  function ctrlAltDel() {
    const session = sessionRef.current;
    if (session == null) {
      return;
    }
    for (const message of CTRL_ALT_DEL) {
      session.sendInput(message);
    }
    videoRef.current?.focus();
  }

  async function disconnect() {
    if (document.fullscreenElement != null) {
      await document.exitFullscreen();
    }
    sessionRef.current?.close();
    sessionRef.current = null;
    clearModeBRequest();
    navigate('/');
  }

  if (hostname == null) {
    return (
      <main className="connect">
        <form
          className="card"
          onSubmit={(event) => {
            event.preventDefault();
            navigate('/');
          }}
        >
          <h1>No session</h1>
          <p className="lede">Start from the connect screen.</p>
          <button type="submit">Back</button>
        </form>
      </main>
    );
  }

  const link = linkState(state);

  return (
    <div className="session">
      <div ref={screenRef} className="screen">
        <header
          className={`bar${fullscreen && barPeek ? ' peek' : ''}`}
          onMouseLeave={() => {
            if (fullscreen) {
              setBarPeek(false);
            }
          }}
        >
          <span className="mark">Racko</span>
          <span className={`link-state ${link}`}>
            <span className="dot" aria-hidden="true" />
            {linkLabel(link)}
          </span>
          <span className={`status${link === 'error' ? ' failed' : ''}`}>{statusText(hostname, state)}</span>
          <div className="actions">
            <button type="button" onClick={() => setFit((value) => !value)}>
              {fit ? 'Actual size' : 'Fit'}
            </button>
            <button type="button" onClick={() => void toggleFullscreen()}>
              {fullscreen ? 'Exit full-screen' : 'Full-screen'}
            </button>
            <button type="button" onClick={ctrlAltDel} disabled={state.input !== 'open'}>
              Ctrl+Alt+Del
            </button>
            <button type="button" className="danger" onClick={() => void disconnect()}>
              Disconnect
            </button>
          </div>
        </header>
        {fullscreen && !barPeek ? <div className="edge" onMouseEnter={() => setBarPeek(true)} /> : null}
        <div className="stage-wrap">
          <div className="stage modeb-stage">
            <video
              ref={videoRef}
              className={fit ? 'fit' : undefined}
              autoPlay
              muted
              playsInline
              tabIndex={0}
            />
          </div>
          {link === 'lost' || link === 'error' ? (
            <div className="reconnect-overlay">
              <div className="reconnect-card">
                <h2>{overlayTitle(link, state)}</h2>
                <p>
                  {state.error?.startsWith(AUTH_FAILED)
                    ? 'The desktop rejected the credentials. Check them on the connect screen.'
                    : (state.error ?? 'The GPU stream closed.')}
                </p>
                <Link to="/" className="back-link" onClick={clearModeBRequest}>
                  Back to connect
                </Link>
              </div>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

function linkState(state: ModeBState): LinkState {
  if (state.error != null || state.peer === 'failed' || state.ice === 'failed') {
    return 'error';
  }
  if (state.signaling === 'closed' || state.peer === 'disconnected' || state.peer === 'closed') {
    return 'lost';
  }
  return state.peer === 'connected' ? 'connected' : 'connecting';
}

function overlayTitle(link: LinkState, state: ModeBState): string {
  if (state.error?.startsWith(AUTH_FAILED)) {
    return AUTH_FAILED;
  }
  return link === 'error' ? 'Connection error' : 'Stream ended';
}

function linkLabel(link: LinkState): string {
  switch (link) {
    case 'connecting':
      return 'Connecting';
    case 'connected':
      return 'Connected';
    case 'lost':
      return 'Lost';
    case 'error':
      return 'Error';
  }
}

function statusText(hostname: string, state: ModeBState): string {
  const size = state.video == null ? '' : ` · ${state.video.width}×${state.video.height}`;
  const route = state.route == null ? '' : ` via ${state.route}`;
  return `${hostname}${size} · ICE ${state.ice}${route} · peer ${state.peer} · input ${state.input}`;
}
