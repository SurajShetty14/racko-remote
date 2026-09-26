/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_GATEWAY_URL?: string;
  readonly VITE_BROKER_API_BASE?: string;
}

declare module '@devolutions/iron-remote-desktop-rdp' {
  export const Backend: object;
  export function init(logLevel: string): Promise<void>;
  export function displayControl(enable: boolean): unknown;
  export function disableWallpaper(disable: boolean): unknown;
  export function disableMenuAnimations(disable: boolean): unknown;
  export function disableFontSmoothing(disable: boolean): unknown;
}
