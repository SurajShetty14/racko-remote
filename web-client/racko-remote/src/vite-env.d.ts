/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_GATEWAY_URL?: string;
  readonly VITE_BROKER_API_BASE?: string;
  readonly VITE_MODEB_SIGNALING_URL?: string;
  readonly VITE_DEV_RDP_HOST?: string;
  readonly VITE_DEV_RDP_USER?: string;
}

declare module '@devolutions/iron-remote-desktop-rdp' {
  export const Backend: object;
  export function init(logLevel: string): Promise<void>;
  export function displayControl(enable: boolean): unknown;
  export function disableWallpaper(disable: boolean): unknown;
  export function disableMenuAnimations(disable: boolean): unknown;
  export function disableFontSmoothing(disable: boolean): unknown;
}
