import { useEffect, useRef, useState, type RefObject } from 'react';
import { Link, useNavigate } from 'react-router-dom';

import {
  ScreenScale,
  connectSession,
  createDesktopElement,
  errorMessage,
  initRdp,
  waitUntilReady,
  type IronRemoteDesktopElement,
  type NewSessionInfo,
  type UserInteraction,
} from '../rdp';
import { clearConnectRequest, readConnectRequest, type ConnectRequest } from '../session-store';

/** Delays before retries 1..5 after an unexpected socket drop. */
const RETRY_DELAYS_MS = [1000, 2000, 4000, 8000, 16000];

type LinkState = 'connecting' | 'connected' | 'reconnecting' | 'lost' | 'error';

export function SessionPage() {
  const navigate = useNavigate();
  const screenRef = useRef<HTMLDivElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const uiRef = useRef<UserInteraction | null>(null);
  const scaleRef = useRef<number>(ScreenScale.Fit);
  const [status, setStatus] = useState('Connecting…');
  const [failed, setFailed] = useState(false);
  const [fullscreen, setFullscreen] = useState(false);
  const [barPeek, setBarPeek] = useState(false);
  const [link, setLink] = useState<LinkState>('connecting');
  const [retry, setRetry] = useState(0);
  const [generation, setGeneration] = useState(0);
  const userLeft = useRef(false);
  const request = readConnectRequest();

  useEffect(() => {
    if (request == null) {
      return;
    }

    const abort = new AbortController();
    userLeft.current = false;
    let element: IronRemoteDesktopElement | null = null;

    const stop = () => {
      try {
        uiRef.current?.shutdown();
      } catch {
        // The socket may already be gone.
      }
      uiRef.current = null;
      element?.remove();
      element = null;
    };

    void runConnection(request, abort.signal, userLeft, {
      setStatus,
      setFailed,
      setLink,
      setRetry,
      stageRef,
      uiRef,
      mount(next) {
        element = next;
      },
      stop,
    });

    return () => {
      abort.abort();
      stop();
    };
  }, [request, generation]);

  useEffect(() => {
    const screen = screenRef.current;

    function applyFitAfterLayout() {
      requestAnimationFrame(() => {
        uiRef.current?.setScale(ScreenScale.Fit);
      });
    }

    function onFullscreenChange() {
      const active = screen != null && document.fullscreenElement === screen;
      setFullscreen(active);
      setBarPeek(false);
      if (active) {
        applyFitAfterLayout();
      } else {
        requestAnimationFrame(() => {
          uiRef.current?.setScale(scaleRef.current);
        });
      }
    }

    document.addEventListener('fullscreenchange', onFullscreenChange);
    return () => document.removeEventListener('fullscreenchange', onFullscreenChange);
  }, []);

  function fit() {
    scaleRef.current = ScreenScale.Fit;
    uiRef.current?.setScale(ScreenScale.Fit);
  }

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
    } catch (error) {
      console.error('Fullscreen was blocked', error);
    }
  }

  async function disconnect() {
    userLeft.current = true;
    if (document.fullscreenElement != null) {
      await document.exitFullscreen();
    }
    try {
      uiRef.current?.shutdown();
    } catch {
      // Already closed.
    }
    clearConnectRequest();
    navigate('/');
  }

  if (request == null) {
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
          <span className={`status${failed ? ' failed' : ''}`}>{status}</span>
          <div className="actions">
            <button type="button" onClick={fit}>
              Fit
            </button>
            <button type="button" onClick={() => void toggleFullscreen()}>
              {fullscreen ? 'Exit full-screen' : 'Full-screen'}
            </button>
            <button type="button" onClick={() => uiRef.current?.ctrlAltDel()}>
              Ctrl+Alt+Del
            </button>
            <button type="button" className="danger" onClick={() => void disconnect()}>
              Disconnect
            </button>
          </div>
        </header>
        {fullscreen && !barPeek ? <div className="edge" onMouseEnter={() => setBarPeek(true)} /> : null}
        <div className="stage-wrap">
          <div ref={stageRef} className="stage" />
          {link === 'reconnecting' ? (
            <div className="reconnect-overlay">
              <div className="reconnect-card">
                <h2>Reconnecting…</h2>
                <p>
                  Attempt {retry} of {RETRY_DELAYS_MS.length}
                </p>
              </div>
            </div>
          ) : null}
          {link === 'lost' || link === 'error' ? (
            <div className="reconnect-overlay">
              <div className="reconnect-card">
                <h2>{link === 'error' ? 'Connection error' : 'Connection lost'}</h2>
                <p>
                  {link === 'error'
                    ? 'The desktop stream could not be decoded. Reconnect when you are ready.'
                    : 'The session could not be restored.'}
                </p>
                <button type="button" onClick={() => setGeneration((value) => value + 1)}>
                  Reconnect
                </button>
                <Link
                  to="/"
                  className="back-link"
                  onClick={() => {
                    userLeft.current = true;
                    clearConnectRequest();
                  }}
                >
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

function isGraphicsFailure(message: string): boolean {
  return /progressive|rfx|srl\(|decode failed|bitmap update|codec/i.test(message);
}

function showGraphicsError(
  setFailed: (failed: boolean) => void,
  setLink: (link: LinkState) => void,
  setStatus: (status: string) => void,
): void {
  setFailed(true);
  setLink('error');
  setStatus('Connection error');
}

function linkLabel(link: LinkState): string {
  switch (link) {
    case 'connecting':
      return 'Connecting';
    case 'connected':
      return 'Connected';
    case 'reconnecting':
      return 'Reconnecting';
    case 'lost':
      return 'Lost';
    case 'error':
      return 'Error';
  }
}

function delay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const timer = window.setTimeout(resolve, ms);
    signal.addEventListener(
      'abort',
      () => {
        window.clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

type ConnectionHooks = {
  setStatus: (status: string) => void;
  setFailed: (failed: boolean) => void;
  setLink: (link: LinkState) => void;
  setRetry: (retry: number) => void;
  stageRef: RefObject<HTMLDivElement | null>;
  uiRef: RefObject<UserInteraction | null>;
  mount: (element: IronRemoteDesktopElement) => void;
  stop: () => void;
};

async function runConnection(
  request: ConnectRequest,
  signal: AbortSignal,
  userLeft: RefObject<boolean>,
  hooks: ConnectionHooks,
): Promise<void> {
  const { setStatus, setFailed, setLink, setRetry, stageRef, uiRef, mount, stop } = hooks;
  let unexpectedEnds = 0;

  while (!signal.aborted && !userLeft.current) {
    const isRetry = unexpectedEnds > 0;
    setFailed(false);
    setLink(isRetry ? 'reconnecting' : 'connecting');
    setRetry(unexpectedEnds);
    setStatus(isRetry ? 'Reconnecting…' : 'Connecting…');

    try {
      const session = await openSession(request, signal, userLeft, stageRef, uiRef, mount, stop);
      if (session == null || signal.aborted || userLeft.current) {
        return;
      }
      unexpectedEnds = 0;
      setLink('connected');
      setRetry(0);
      setStatus(request.hostname);
      try {
        const info = await session.run();
        if (signal.aborted || userLeft.current) {
          return;
        }
        if (isGraphicsFailure(info.reason())) {
          showGraphicsError(setFailed, setLink, setStatus);
          return;
        }
        setStatus(info.reason());
      } catch (error) {
        if (signal.aborted || userLeft.current) {
          return;
        }
        const message = errorMessage(error);
        if (isGraphicsFailure(message)) {
          showGraphicsError(setFailed, setLink, setStatus);
          return;
        }
        setStatus(message);
      }
    } catch (error) {
      if (signal.aborted || userLeft.current) {
        return;
      }
      const message = errorMessage(error);
      if (isGraphicsFailure(message)) {
        showGraphicsError(setFailed, setLink, setStatus);
        return;
      }
      setStatus(message);
    }

    if (signal.aborted || userLeft.current) {
      return;
    }
    unexpectedEnds += 1;
    if (unexpectedEnds > RETRY_DELAYS_MS.length) {
      setLink('lost');
      setFailed(true);
      setStatus('Connection lost');
      return;
    }
    setLink('reconnecting');
    setRetry(unexpectedEnds);
    setStatus('Reconnecting…');
    await delay(RETRY_DELAYS_MS[unexpectedEnds - 1], signal);
  }
}

async function openSession(
  request: ConnectRequest,
  signal: AbortSignal,
  userLeft: RefObject<boolean>,
  stageRef: RefObject<HTMLDivElement | null>,
  uiRef: RefObject<UserInteraction | null>,
  mount: (element: IronRemoteDesktopElement) => void,
  stop: () => void,
): Promise<NewSessionInfo | null> {
  const stage = stageRef.current;
  if (stage == null) {
    throw new Error('Session stage is missing');
  }
  stop();
  const element = createDesktopElement();
  mount(element);
  const ready = waitUntilReady(element);
  stage.appendChild(element);
  await initRdp();
  if (signal.aborted || userLeft.current) {
    return null;
  }
  const ui = await ready;
  if (signal.aborted || userLeft.current) {
    try {
      ui.shutdown();
    } catch {
      // Already closed.
    }
    return null;
  }
  uiRef.current = ui;
  const session = await connectSession(ui, request, element);
  if (signal.aborted || userLeft.current) {
    return null;
  }
  return session;
}
