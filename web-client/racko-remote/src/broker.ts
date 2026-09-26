/**
 * Management API origin. Dev is the broker's :9090 listener.
 * Production (nginx) is `https://<domain>/api`, which already includes `/api`.
 */
export const BROKER_BASE_URL = (
  import.meta.env.VITE_BROKER_API_BASE || 'http://localhost:9090'
).replace(/\/$/, '');

/** `path` is an app path such as `/api/sessions`. */
export function brokerUrl(path: string): string {
  const suffix = path.startsWith('/') ? path : `/${path}`;
  if (BROKER_BASE_URL.endsWith('/api') && suffix.startsWith('/api/')) {
    return `${BROKER_BASE_URL}${suffix.slice('/api'.length)}`;
  }
  return `${BROKER_BASE_URL}${suffix}`;
}

export type BrokerSession = {
  id: string;
  dest: string;
  clientIp: string;
  startedAt: string;
  durationSecs: number;
  bytesSent: number;
  bytesReceived: number;
};
