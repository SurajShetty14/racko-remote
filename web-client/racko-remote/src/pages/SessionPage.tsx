import { useEffect, useRef, useState, type RefObject } from 'react';
import { Link, useNavigate } from 'react-router-dom';

import {
  IronErrorKind,
  ScreenScale,
  connectSession,
  createDesktopElement,
  errorKind,
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

/** A session must stay up this long before a drop earns a fresh retry budget. */
const STABLE_SESSION_MS = 60_000;

type LinkState = 'connecting' | 'connected' | 'reconnecting' | 'failed';

/** A terminal state: auto-reconnect stops and the user decides whether to retry. */
type Failure = { label: string; title: string; detail: string };

const FAILURES = {
  graphics: {
    label: 'Error',
    title: 'Connection error',
    detail: 'The desktop stream could not be decoded. Reconnect when you are ready.',
  },
  takeover: {
    label: 'Taken over',
    title: 'Session taken over by another connection',
    detail: 'Someone else signed in to this desktop. Reconnecting will disconnect them.',
  },
  auth: {
    label: 'Sign-in failed',
    title: 'Authentication failed',
    detail: 'The desktop rejected the credentials. Check them on the connect screen.',
  },
  refused: {
    label: 'Refused',
    title: 'Connection refused',
    detail: 'The gateway could not open this desktop: it is not on the allowed list, or the desktop refused the connection.',
  },
  lost: {
    label: 'Lost',
    title: 'Connection lost',
    detail: `The session could not be restored after ${RETRY_DELAYS_MS.length} attempts.`,
  },
} satisfies Record<string, Failure>;

export function SessionPage() {
  const navigate = useNavigate();
  const screenRef = useRef<HTMLDivElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const uiRef = useRef<UserInteraction | null>(null);
  const scaleRef = useRef<number>(ScreenScale.Fit);
  const [status, setStatus] = useState('Connecting…');
  const [failure, setFailure] = useState<Failure | null>(null);
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
      setFailure,
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
            {failure?.label ?? linkLabel(link)}
          </span>
          <span className={`status${failure != null ? ' failed' : ''}`}>{status}</span>
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
          {failure != null ? (
            <div className="reconnect-overlay">
              <div className="reconnect-card">
                <h2>{failure.title}</h2>
                <p>{failure.detail}</p>
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

/**
 * Server-sent disconnect reasons meaning another connection or an admin took the session.
 * Retrying would just kick the other side (or get kicked again), so these are not retried.
 * Matches the ironrdp-pdu `ProtocolIndependentCode` descriptions for
 * DisconnectedByOtherconnection, RpcInitiatedLogoff and RpcInitiatedDisconnect.
 */
function isSessionTakeover(message: string): boolean {
  return /another user connected to the server|forced logoff|on the server in another session/i.test(message);
}

/**
 * Classifies a thrown connect or session error. `null` means a transport drop worth retrying.
 * The broker answers a disallowed destination, a refused or timed-out TCP connect, and a failed
 * X.224 or TLS handshake with an RDCleanPath error, so the `RDCleanPath` kind covers all of them.
 * `ProxyConnect` (the gateway WebSocket failed to open) stays retryable.
 */
function classifyError(error: unknown): Failure | null {
  const message = errorMessage(error);
  if (isGraphicsFailure(message)) {
    return FAILURES.graphics;
  }
  if (isSessionTakeover(message)) {
    return FAILURES.takeover;
  }
  switch (errorKind(error)) {
    case IronErrorKind.WrongPassword:
    case IronErrorKind.LogonFailure:
    case IronErrorKind.AccessDenied:
      return FAILURES.auth;
    case IronErrorKind.RDCleanPath:
    case IronErrorKind.NegotiationFailure:
      return FAILURES.refused;
  }
  if (/logon failure|authentication failed|wrong password|access denied|STATUS_LOGON_FAILURE/i.test(message)) {
    return FAILURES.auth;
  }
  if (/connection refused|actively refused|ECONNREFUSED|not allowed|forbidden/i.test(message)) {
    return FAILURES.refused;
  }
  return null;
}

/**
 * Classifies a graceful end of `session.run()`. The server chose to end the session
 * (logoff, idle limit, admin disconnect, takeover), so none of these are retried.
 */
function classifyDisconnect(reason: string): Failure {
  if (isGraphicsFailure(reason)) {
    return FAILURES.graphics;
  }
  if (isSessionTakeover(reason)) {
    return FAILURES.takeover;
  }
  return { label: 'Ended', title: 'Session ended by the desktop', detail: reason };
}

function linkLabel(link: LinkState): string {
  switch (link) {
    case 'connecting':
      return 'Connecting';
    case 'connected':
      return 'Connected';
    case 'reconnecting':
      return 'Reconnecting';
    case 'failed':
      return 'Failed';
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
  setFailure: (failure: Failure | null) => void;
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
  const { setStatus, setFailure, setLink, setRetry, stageRef, uiRef, mount, stop } = hooks;
  const ended = () => signal.aborted || userLeft.current;
  const fail = (failure: Failure) => {
    setFailure(failure);
    setLink('failed');
    setStatus(failure.title);
  };
  let unexpectedEnds = 0;

  while (!ended()) {
    const isRetry = unexpectedEnds > 0;
    setFailure(null);
    setLink(isRetry ? 'reconnecting' : 'connecting');
    setRetry(unexpectedEnds);
    setStatus(isRetry ? 'Reconnecting…' : 'Connecting…');

    let failure: Failure | null = null;
    try {
      const session = await openSession(request, signal, userLeft, stageRef, uiRef, mount, stop);
      if (session == null || ended()) {
        return;
      }
      setLink('connected');
      setRetry(0);
      setStatus(request.hostname);
      const connectedAt = performance.now();
      try {
        const info = await session.run();
        if (ended()) {
          return;
        }
        failure = classifyDisconnect(info.reason());
      } catch (error) {
        if (ended()) {
          return;
        }
        failure = classifyError(error);
        console.warn('RDP session dropped', errorMessage(error));
      }
      // A connect-then-drop cycle must not reset the budget, or it loops forever.
      if (performance.now() - connectedAt >= STABLE_SESSION_MS) {
        unexpectedEnds = 0;
      }
    } catch (error) {
      if (ended()) {
        return;
      }
      failure = classifyError(error);
      console.warn('RDP connect failed', errorMessage(error));
    }

    if (ended()) {
      return;
    }
    if (failure != null) {
      fail(failure);
      return;
    }
    unexpectedEnds += 1;
    if (unexpectedEnds > RETRY_DELAYS_MS.length) {
      fail(FAILURES.lost);
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
