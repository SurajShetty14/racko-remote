import { BROKER_BASE_URL } from './broker';

/**
 * Mode B signaling on the main broker. Defaults to the broker API origin:
 * `ws://localhost:9090/modeb/webrtc` in dev, `wss://<domain>/modeb/webrtc` behind nginx.
 */
export const MODEB_SIGNALING_URL: string =
  import.meta.env.VITE_MODEB_SIGNALING_URL || defaultSignalingUrl();

function defaultSignalingUrl(): string {
  const url = new URL(BROKER_BASE_URL, window.location.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.pathname = '/modeb/webrtc';
  url.search = '';
  return url.toString();
}

/**
 * Handed from the connect page to the Mode B session page. Kept in memory rather than router
 * state, which the browser persists in session history.
 */
export type ModeBRequest = {
  hostname: string;
  username: string;
  password: string;
  domain: string;
  /** Requested RDP desktop size in device pixels, from {@link modeBDesktopSize}. */
  width: number;
  height: number;
};

const MAX_DESKTOP_WIDTH = 1920;
const MAX_DESKTOP_HEIGHT = 1200;

/**
 * This screen's size in device pixels, scaled down to fit 1920×1200 with its aspect ratio kept
 * (clamping each side alone would reintroduce letterboxing), and rounded down to even numbers
 * for the H.264 encoder. The broker applies the same limits.
 */
export function modeBDesktopSize(): { width: number; height: number } {
  const dpr = window.devicePixelRatio || 1;
  let width = Math.max(1, Math.floor(window.screen.width * dpr));
  let height = Math.max(1, Math.floor(window.screen.height * dpr));
  if (width > MAX_DESKTOP_WIDTH) {
    height = Math.floor((height * MAX_DESKTOP_WIDTH) / width);
    width = MAX_DESKTOP_WIDTH;
  }
  if (height > MAX_DESKTOP_HEIGHT) {
    width = Math.floor((width * MAX_DESKTOP_HEIGHT) / height);
    height = MAX_DESKTOP_HEIGHT;
  }
  return { width: width & ~1, height: height & ~1 };
}

let pendingRequest: ModeBRequest | null = null;

export function saveModeBRequest(request: ModeBRequest): void {
  pendingRequest = request;
}

export function readModeBRequest(): ModeBRequest | null {
  return pendingRequest;
}

export function clearModeBRequest(): void {
  pendingRequest = null;
}

/** Prefix of the broker's error for missing, malformed, or rejected credentials. */
export const AUTH_FAILED = 'Authentication failed';

type MouseButtonName = 'left' | 'middle' | 'right';

/** Input messages in the exact shapes the broker's Mode B input translator parses. */
export type InputMessage =
  | { type: 'mousemove'; x: number; y: number }
  | { type: 'mousedown' | 'mouseup'; button: MouseButtonName; x: number; y: number }
  | { type: 'wheel'; deltaX: number; deltaY: number; deltaMode: number }
  | { type: 'keydown'; code: string; repeat: boolean }
  | { type: 'keyup'; code: string };

type SignalingMessage =
  | { type: 'auth'; username: string; password: string; domain: string; width: number; height: number }
  | { type: 'offer' | 'answer'; sdp: string }
  | { type: 'ice'; candidate: string; sdpMLineIndex: number | null }
  | { type: 'ice-servers'; iceServers: RTCIceServer[] }
  | { type: 'error'; message: string };

export type ModeBState = {
  signaling: 'connecting' | 'open' | 'closed';
  ice: RTCIceConnectionState;
  /** Local candidate type of the selected pair: `host`, `srflx`, `prflx`, or `relay` (TURN). */
  route: string | null;
  peer: RTCPeerConnectionState;
  input: RTCDataChannelState;
  video: { width: number; height: number } | null;
  error: string | null;
};

export type ModeBSessionHandle = {
  sendInput: (message: InputMessage) => void;
  /** WebRTC stats of the peer connection, for quality diagnostics. */
  getStats: () => Promise<RTCStatsReport>;
  close: () => void;
};

export const INITIAL_MODEB_STATE: ModeBState = {
  signaling: 'connecting',
  ice: 'new',
  route: null,
  peer: 'new',
  input: 'connecting',
  video: null,
  error: null,
};

const BUTTONS: readonly MouseButtonName[] = ['left', 'middle', 'right'];

/**
 * Opens a Mode B WebRTC session to `request.hostname` (host:port): signaling WebSocket,
 * receive-only H.264 video into `video`, and the pre-negotiated "input" data channel fed by mouse
 * and keyboard events on `video`. The credentials go in the first signaling message, never the URL;
 * the broker signs in to the desktop with them.
 */
export function openModeBSession(
  video: HTMLVideoElement,
  request: ModeBRequest,
  onState: (state: ModeBState) => void,
): ModeBSessionHandle {
  let state = INITIAL_MODEB_STATE;
  let closed = false;
  const update = (patch: Partial<ModeBState>) => {
    if (closed) {
      return;
    }
    state = { ...state, ...patch };
    onState(state);
  };

  const signalingUrl = new URL(MODEB_SIGNALING_URL);
  signalingUrl.searchParams.set('dest', request.hostname);
  const ws = new WebSocket(signalingUrl);
  const send = (message: SignalingMessage) => {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(message));
    }
  };

  // The broker sends this session's minted STUN/TURN servers before its offer ("ice-servers").
  const pc = new RTCPeerConnection();

  // The broker's webrtcbin creates the same pre-negotiated channel (label "input", id 0) before
  // its offer; creating it here before the answer binds this side to that SCTP stream.
  const input = pc.createDataChannel('input', { ordered: true, negotiated: true, id: 0 });
  const sendInput = (message: InputMessage) => {
    if (input.readyState === 'open') {
      input.send(JSON.stringify(message));
    }
  };
  input.onopen = () => update({ input: input.readyState });
  input.onclose = () => update({ input: input.readyState });
  input.onerror = (event) => console.warn('Mode B input channel error', event);

  pc.oniceconnectionstatechange = () => {
    console.info('Mode B ICE connection state:', pc.iceConnectionState);
    update({ ice: pc.iceConnectionState });
    if (pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed') {
      void selectedRoute().then((route) => {
        console.info('Mode B ICE route:', route);
        update({ route });
      });
    }
  };
  pc.onconnectionstatechange = () => {
    console.info('Mode B peer connection state:', pc.connectionState);
    update({ peer: pc.connectionState });
  };
  pc.onicecandidate = ({ candidate }) => {
    send(
      candidate == null
        ? { type: 'ice', candidate: '', sdpMLineIndex: 0 }
        : { type: 'ice', candidate: candidate.candidate, sdpMLineIndex: candidate.sdpMLineIndex ?? 0 },
    );
  };
  pc.ontrack = ({ track, streams }) => {
    video.srcObject = streams[0] ?? new MediaStream([track]);
    video.play().catch((error: unknown) => console.warn('Mode B video.play() failed', error));
  };

  ws.onopen = () => {
    send({
      type: 'auth',
      username: request.username,
      password: request.password,
      domain: request.domain,
      width: request.width,
      height: request.height,
    });
    update({ signaling: 'open' });
  };
  ws.onclose = () => update({ signaling: 'closed' });
  ws.onerror = () => {
    if (state.signaling === 'connecting') {
      update({ error: `Cannot reach ${MODEB_SIGNALING_URL}` });
    }
  };
  ws.onmessage = (event: MessageEvent<string>) => void onSignal(event.data);

  const listeners = new AbortController();
  const { signal } = listeners;
  const onVideoSize = () => update({ video: { width: video.videoWidth, height: video.videoHeight } });
  video.addEventListener('loadedmetadata', onVideoSize, { signal });
  video.addEventListener('resize', onVideoSize, { signal });

  let pendingMove: { x: number; y: number } | null = null;
  let moveFrame = 0;
  video.addEventListener(
    'mousemove',
    (event) => {
      pendingMove = desktopPoint(event);
      if (moveFrame !== 0) {
        return;
      }
      moveFrame = requestAnimationFrame(() => {
        moveFrame = 0;
        if (pendingMove != null) {
          sendInput({ type: 'mousemove', ...pendingMove });
        }
        pendingMove = null;
      });
    },
    { signal },
  );

  const onButton = (event: MouseEvent) => {
    const type = event.type === 'mousedown' ? 'mousedown' : 'mouseup';
    if (type === 'mousedown') {
      video.focus();
    }
    const button = BUTTONS[event.button] as MouseButtonName | undefined;
    const point = desktopPoint(event);
    if (button == null || point == null) {
      return;
    }
    event.preventDefault();
    pendingMove = null;
    sendInput({ type, button, ...point });
  };
  video.addEventListener('mousedown', onButton, { signal });
  video.addEventListener('mouseup', onButton, { signal });
  video.addEventListener('contextmenu', (event) => event.preventDefault(), { signal });

  video.addEventListener(
    'wheel',
    (event) => {
      event.preventDefault();
      sendInput({ type: 'wheel', deltaX: event.deltaX, deltaY: event.deltaY, deltaMode: event.deltaMode });
    },
    { passive: false, signal },
  );

  // Keys still down when focus leaves (Alt+Tab, clicking the toolbar) never get a keyup here,
  // so they are released explicitly to avoid stuck modifiers on the remote desktop.
  const heldKeys = new Set<string>();
  const releaseHeldKeys = () => {
    for (const code of heldKeys) {
      sendInput({ type: 'keyup', code });
    }
    heldKeys.clear();
  };
  const onKey = (event: KeyboardEvent) => {
    event.preventDefault();
    if (event.type === 'keydown') {
      heldKeys.add(event.code);
      sendInput({ type: 'keydown', code: event.code, repeat: event.repeat });
    } else {
      heldKeys.delete(event.code);
      sendInput({ type: 'keyup', code: event.code });
    }
  };
  video.addEventListener('keydown', onKey, { signal });
  video.addEventListener('keyup', onKey, { signal });
  video.addEventListener('blur', releaseHeldKeys, { signal });

  return {
    sendInput,
    getStats: () => pc.getStats(),
    close() {
      releaseHeldKeys();
      closed = true;
      listeners.abort();
      cancelAnimationFrame(moveFrame);
      ws.close();
      pc.close();
      video.srcObject = null;
    },
  };

  async function onSignal(data: string) {
    try {
      const message = JSON.parse(data) as SignalingMessage;
      if (message.type === 'ice-servers') {
        // Before the answer, so gathering for it already uses TURN.
        pc.setConfiguration({ iceServers: message.iceServers });
        console.info('Mode B ICE servers from broker:', message.iceServers.length);
      } else if (message.type === 'offer') {
        await pc.setRemoteDescription({ type: 'offer', sdp: message.sdp });
        const answer = await pc.createAnswer();
        await pc.setLocalDescription(answer);
        send({ type: 'answer', sdp: pc.localDescription?.sdp ?? answer.sdp ?? '' });
      } else if (message.type === 'ice') {
        await pc.addIceCandidate({ candidate: message.candidate, sdpMLineIndex: message.sdpMLineIndex });
      } else if (message.type === 'error') {
        update({ error: message.message });
      } else {
        console.warn('Unexpected Mode B signaling message', data);
      }
    } catch (error) {
      if (!closed) {
        console.error('Mode B signaling handling failed', error);
      }
    }
  }

  async function selectedRoute(): Promise<string | null> {
    try {
      const stats = await pc.getStats();
      let pairId: string | undefined;
      stats.forEach((report) => {
        // Chrome/Safari name the pair on the transport; Firefox marks the pair itself.
        if (report.type === 'transport' && report.selectedCandidatePairId != null) {
          pairId = report.selectedCandidatePairId;
        } else if (
          report.type === 'candidate-pair' &&
          pairId == null &&
          (report.selected === true || (report.nominated === true && report.state === 'succeeded'))
        ) {
          pairId = report.id;
        }
      });
      const pair = pairId == null ? undefined : stats.get(pairId);
      return pair == null ? null : (stats.get(pair.localCandidateId)?.candidateType ?? null);
    } catch {
      return null;
    }
  }

  // Maps a pointer event to desktop pixels (videoWidth x videoHeight is the RDP desktop size),
  // accounting for letterboxing from object-fit: contain.
  function desktopPoint(event: MouseEvent): { x: number; y: number } | null {
    const vw = video.videoWidth;
    const vh = video.videoHeight;
    if (vw === 0 || vh === 0) {
      return null;
    }
    const rect = video.getBoundingClientRect();
    const scale = Math.min(rect.width / vw, rect.height / vh);
    const left = rect.left + (rect.width - vw * scale) / 2;
    const top = rect.top + (rect.height - vh * scale) / 2;
    const clamp = (value: number, max: number) => Math.min(Math.max(Math.round(value), 0), max - 1);
    return { x: clamp((event.clientX - left) / scale, vw), y: clamp((event.clientY - top) / scale, vh) };
  }
}
