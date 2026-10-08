<script lang="ts">
  import { event as loadEvent } from '$lib/api/client';
  import type { EventDetail } from '$lib/api/types';
  import { about, clockTime, prettyPayload } from '$lib/format';
  import Icon from '$lib/ui/Icon.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';
  import Outcomes from './Outcomes.svelte';

  /** One event beside the list (#227): its facts, what each listener did,
   * and the payload: laid out to read when it is JSON, as received on
   * request, and copied as received. While no listener has answered, it is
   * read again every `every` ms, for at most `patience` ms. */
  let {
    id,
    load = loadEvent,
    every = 2000,
    patience = 30000,
    copy = (text: string) => navigator.clipboard.writeText(text),
  }: {
    id: string;
    load?: (id: string) => Promise<EventDetail>;
    every?: number;
    patience?: number;
    copy?: (text: string) => Promise<void>;
  } = $props();

  let detail: EventDetail | null = $state(null);
  let problem: unknown = $state(null);
  let copied = $state(false);
  let asReceived = $state(false);

  $effect(() => {
    const wanted = id;
    const until = Date.now() + patience;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let live = true;
    detail = null;
    problem = null;
    copied = false;
    asReceived = false;
    const read = async (): Promise<void> => {
      try {
        const got = await load(wanted);
        if (!live) return;
        detail = got;
        if (got.outcomes.length === 0 && Date.now() < until) {
          timer = setTimeout(() => void read(), every);
        }
      } catch (error) {
        if (live) problem = error;
      }
    };
    void read();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  });

  async function copyPayload(text: string): Promise<void> {
    try {
      await copy(text);
      copied = true;
    } catch {
      // The clipboard said no; the text is there to select.
    }
  }
</script>

<section class="panel inspector" aria-label="The event chosen">
  {#if problem !== null}
    <Problem error={problem} />
  {:else if detail === null}
    <Loading />
  {:else}
    {@const event = detail.event}
    <div class="panel-head">
      <h2><span class="label">Event</span> <span class="mono">{event.id}</span></h2>
    </div>
    <div class="panel-body">
      <dl class="facts">
        <div>
          <dt>Received</dt>
          <dd><Time iso={event.received_at} /> <span class="muted mono">{clockTime(event.received_at)}</span></dd>
        </div>
        <div><dt>Source</dt><dd class="mono">{event.source}</dd></div>
        <div><dt>Kind</dt><dd class="mono">{event.kind}</dd></div>
        <div>
          <dt>About</dt>
          <dd>{#if event.repo}{about(event.repo, event.target)}{:else}<span class="muted">no repository</span>{/if}</dd>
        </div>
        <div>
          <dt>Asked by</dt>
          <dd>{#if event.requester}<span class="mono">{event.requester}</span>{:else}<span class="muted">not known</span>{/if}</dd>
        </div>
      </dl>

      <h3>What the listeners did</h3>
      {#if detail.outcomes.length === 0}
        <p class="muted" aria-busy="true"><Status word="waiting" /> No listener has answered yet.</p>
      {:else}
        <Outcomes outcomes={detail.outcomes} />
      {/if}

      {#if detail.payload !== null}
        {@const raw = detail.payload}
        {@const pretty = prettyPayload(raw)}
        {@const laidOut = pretty !== raw && !asReceived}
        <div class="payload-head">
          <h3>
            {laidOut ? 'Payload, laid out to read' : 'Payload as received'}
            <span class="data-note">Other people's text, shown as data</span>
          </h3>
          <div class="payload-actions">
            {#if pretty !== raw}
              <button type="button" class="payload-view" onclick={() => (asReceived = !asReceived)}>
                {asReceived ? 'Lay out' : 'As received'}
              </button>
            {/if}
            <button type="button" class="icon-button" onclick={() => copyPayload(raw)} title="Copy the payload">
              <Icon name={copied ? 'check' : 'copy'} size={14} /><span class="visually-hidden">Copy the payload</span>
            </button>
          </div>
          {#if copied}<span class="visually-hidden" role="status">Copied.</span>{/if}
        </div>
        {#if laidOut}
          <p class="muted note">Parsed and printed again: big numbers, number forms, repeated keys and escapes can differ from what came. Copy gives the payload as received.</p>
        {/if}
        <pre class="payload">{laidOut ? pretty : raw}</pre>
      {:else}
        <p class="muted">No payload was kept.</p>
      {/if}
    </div>
  {/if}
</section>

<style>
  .inspector { position: sticky; top: var(--space-4); max-height: calc(100vh - 2 * var(--space-4)); overflow: auto; }
  .label { font-size: 11.5px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--muted); }
  h3 { font-size: 14px; margin: var(--space-4) 0 var(--space-2); }
  .payload-head { display: flex; align-items: center; justify-content: space-between; gap: var(--space-2); }
  .payload { max-height: 32rem; }
  .payload-actions { display: flex; align-items: center; gap: var(--space-2); }
  .payload-view { font-size: 12.5px; padding: 2px var(--space-2); }
  .note { font-size: 12.5px; margin: 0 0 var(--space-2); }

  @media (max-width: 960px) {
    .inspector { position: static; max-height: none; }
  }
</style>
