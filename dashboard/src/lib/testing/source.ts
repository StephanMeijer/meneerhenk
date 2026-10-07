// A stand-in for EventSource: the test pushes messages into it.
import type { Connect, Source } from '$lib/api/stream';

export class FakeSource implements Source {
  readonly url: string;
  readyState = 1;
  closed = false;
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  private listeners = new Map<string, ((event: MessageEvent<string>) => void)[]>();

  constructor(url: string) {
    this.url = url;
  }

  addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  close(): void {
    this.closed = true;
    this.readyState = 2;
  }

  open(): void {
    this.onopen?.(new Event('open'));
  }

  push(kind: string, data?: unknown): void {
    const event = new MessageEvent<string>(kind, { data: data === undefined ? '' : JSON.stringify(data) });
    for (const listener of this.listeners.get(kind) ?? []) listener(event);
  }
}

/** A `connect` that records every source it opens. */
export function fakeConnect(): { connect: Connect; sources: FakeSource[] } {
  const sources: FakeSource[] = [];
  return {
    sources,
    connect: (url) => {
      const source = new FakeSource(url);
      sources.push(source);
      return source;
    },
  };
}

/** The first source opened; fails the test when none was. */
export function firstOf(sources: FakeSource[]): FakeSource {
  const source = sources[0];
  if (source === undefined) throw new Error('no stream was opened');
  return source;
}
