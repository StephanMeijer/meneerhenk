// Following a stream of /dashboard/api/v1 (docs/API.md): Server-Sent Events
// through the browser's EventSource, which reconnects by itself and sends
// the last id it saw, so the server replays what was missed.
import { API, me } from './client';

/** The part of EventSource a follower uses; tests give a stand-in. */
export interface Source {
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
  close(): void;
  onopen: ((event: Event) => void) | null;
  onerror: ((event: Event) => void) | null;
  readonly readyState: number;
}

/** Opens a stream; replaced in the tests. */
export type Connect = (url: string) => Source;

export const connectEventSource: Connect = (url) => new EventSource(url);

/** EventSource's closed state: it will not reconnect. */
const CLOSED = 2;

/** A stream message: its event name and its data. */
export type Message = { kind: string; data?: unknown };

export interface Following {
  close(): void;
}

/**
 * Follows `path` under the API, calling `onMessage` for each message of
 * the given kinds and `onLive` when the connection opens or drops. Closes
 * itself after an `end` message. A stream the server refused (signed out)
 * is not retried; reading /me then sends the browser to sign-in.
 */
export function follow<M extends Message>(
  path: string,
  kinds: readonly M['kind'][],
  onMessage: (message: M) => void,
  onLive: (live: boolean) => void = () => {},
  connect: Connect = connectEventSource,
): Following {
  const source = connect(API + path);
  for (const kind of kinds) {
    source.addEventListener(kind, (event) => {
      const data: unknown = event.data === undefined || event.data === '' ? undefined : JSON.parse(event.data);
      onMessage({ kind, data } as M);
      if (kind === 'end') {
        source.close();
        onLive(false);
      }
    });
  }
  source.onopen = () => onLive(true);
  source.onerror = () => {
    onLive(false);
    if (source.readyState === CLOSED) {
      void me().catch(() => {});
    }
  };
  return { close: () => source.close() };
}
