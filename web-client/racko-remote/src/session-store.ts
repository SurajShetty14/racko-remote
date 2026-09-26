export interface ConnectRequest {
  hostname: string;
  username: string;
  password: string;
  domain: string;
  desktopWidth: number;
  desktopHeight: number;
}

let pending: ConnectRequest | null = null;

export function saveConnectRequest(request: ConnectRequest): void {
  pending = request;
}

export function readConnectRequest(): ConnectRequest | null {
  return pending;
}

export function clearConnectRequest(): void {
  pending = null;
}
