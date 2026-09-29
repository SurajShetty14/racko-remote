import {
  Backend,
  disableFontSmoothing,
  disableMenuAnimations,
  disableWallpaper,
  displayControl,
  init,
} from '@devolutions/iron-remote-desktop-rdp';

import '@devolutions/iron-remote-desktop';

/** WebSocket the browser opens. Production (nginx) is `wss://<domain>/ws`. */
export const GATEWAY_ADDRESS =
  import.meta.env.VITE_GATEWAY_URL || 'ws://localhost:7171/jet/rdp';
export const AUTH_TOKEN = 'poc';

/** Resolutions the session canvas and the RDP desktop size both use. */
export const DESKTOP_PRESETS = [
  { width: 1280, height: 720 },
  { width: 1920, height: 1080 },
] as const;

export const DESKTOP_SIZE = DESKTOP_PRESETS[0];

/** Mirrors iron-remote-desktop `ScreenScale`. */
export const ScreenScale = {
  Fit: 1,
  Full: 2,
} as const;

export interface ConfigBuilder {
  withUsername(username: string): ConfigBuilder;
  withPassword(password: string): ConfigBuilder;
  withDestination(destination: string): ConfigBuilder;
  withProxyAddress(proxyAddress: string): ConfigBuilder;
  withServerDomain(serverDomain: string): ConfigBuilder;
  withAuthToken(authToken: string): ConfigBuilder;
  withDesktopSize(size: { width: number; height: number }): ConfigBuilder;
  withExtension(ext: unknown): ConfigBuilder;
  build(): unknown;
}

export interface NewSessionInfo {
  initialDesktopSize: { width: number; height: number };
  run(): Promise<{ reason(): string }>;
}

export interface UserInteraction {
  setVisibility(state: boolean): void;
  setScale(scale: number): void;
  setEnableClipboard(enable: boolean): void;
  configBuilder(): ConfigBuilder;
  connect(config: unknown): Promise<NewSessionInfo>;
  ctrlAltDel(): void;
  shutdown(): void;
}

export interface IronRemoteDesktopElement extends HTMLElement {
  verbose: string;
  scale: string;
  flexcenter: string;
  module: object;
}

export function isIronError(error: unknown): error is { backtrace: () => string } {
  return (
    typeof error === 'object' &&
    error !== null &&
    typeof (error as { backtrace?: unknown }).backtrace === 'function'
  );
}

/** Mirrors iron-remote-desktop `IronErrorKind`. */
export const IronErrorKind = {
  General: 0,
  WrongPassword: 1,
  LogonFailure: 2,
  AccessDenied: 3,
  RDCleanPath: 4,
  ProxyConnect: 5,
  NegotiationFailure: 6,
} as const;

export function errorKind(error: unknown): number | null {
  if (typeof error === 'object' && error !== null && typeof (error as { kind?: unknown }).kind === 'function') {
    return (error as { kind: () => number }).kind();
  }
  return null;
}

export function errorMessage(error: unknown): string {
  if (isIronError(error)) {
    return error.backtrace();
  }
  return error instanceof Error ? error.message : String(error);
}

export async function initRdp(): Promise<void> {
  await init('INFO');
}

export function createDesktopElement(): IronRemoteDesktopElement {
  const element = document.createElement('iron-remote-desktop') as IronRemoteDesktopElement;
  element.verbose = 'true';
  element.scale = 'fit';
  element.flexcenter = 'true';
  element.module = Backend;
  return element;
}

export function waitUntilReady(element: IronRemoteDesktopElement): Promise<UserInteraction> {
  return new Promise((resolve) => {
    element.addEventListener(
      'ready',
      (event) => {
        const detail = (event as CustomEvent<{ irgUserInteraction: UserInteraction }>).detail;
        resolve(detail.irgUserInteraction);
      },
      { once: true },
    );
  });
}

export async function connectSession(
  ui: UserInteraction,
  input: {
    hostname: string;
    username: string;
    password: string;
    domain: string;
    desktopWidth: number;
    desktopHeight: number;
  },
  canvasHost: HTMLElement,
): Promise<NewSessionInfo> {
  const desktopSize = { width: input.desktopWidth, height: input.desktopHeight };
  ui.setEnableClipboard(true);
  const config = ui
    .configBuilder()
    .withUsername(input.username)
    .withPassword(input.password)
    .withDestination(input.hostname)
    .withProxyAddress(GATEWAY_ADDRESS)
    .withServerDomain(input.domain)
    .withAuthToken(AUTH_TOKEN)
    .withDesktopSize(desktopSize)
    .withExtension(displayControl(true))
    .withExtension(disableWallpaper(true))
    .withExtension(disableMenuAnimations(true))
    .withExtension(disableFontSmoothing(true))
    .build();

  const session = await ui.connect(config);
  const negotiated = session.initialDesktopSize;
  const canvas = canvasHost.shadowRoot?.querySelector('canvas');
  if (canvas != null) {
    canvas.width = negotiated.width;
    canvas.height = negotiated.height;
  }
  if (negotiated.width !== desktopSize.width || negotiated.height !== desktopSize.height) {
    console.error('Negotiated desktop size does not match the requested canvas size', {
      requested: desktopSize,
      negotiated,
      canvas: canvas == null ? null : { width: canvas.width, height: canvas.height },
    });
  } else {
    console.info('Desktop size matches the canvas backing store', {
      width: negotiated.width,
      height: negotiated.height,
    });
  }
  ui.setVisibility(true);
  return session;
}
