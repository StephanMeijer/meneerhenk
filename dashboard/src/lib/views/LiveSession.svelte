<script lang="ts">
  import { tick } from 'svelte';
  import { runToolCalls } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import type { SessionMessage, ToolCall } from '$lib/api/types';
  import { NO_LOG, applyLive, callsOf, connectionAfter, type Connection, type LiveLog } from '$lib/live';
  import { href, link, transcriptPath } from '$lib/router';
  import Live from '$lib/ui/Live.svelte';
  import MessageParts from '$lib/ui/MessageParts.svelte';
  import Time from '$lib/ui/Time.svelte';

  /** A running session's conversation as it happens (#238): what the
   * model says, its calls and what they return. `streamed` are the calls
   * the run's own stream brought since the page opened; the ones before
   * are loaded once. `connect` and `loadCalls` are replaced in the tests. */
  let {
    runId,
    session,
    streamed = [],
    connect = connectEventSource,
    loadCalls = runToolCalls,
  }: {
    runId: string;
    session: string;
    streamed?: ToolCall[];
    connect?: Connect;
    loadCalls?: (id: string) => Promise<ToolCall[]>;
  } = $props();

  /** How close to the end, in pixels, still counts as at the end. */
  const NEAR_END = 48;

  let log: LiveLog = $state(NO_LOG);
  let connection: Connection = $state('connecting');
  let earlier: ToolCall[] = $state([]);
  let box: HTMLDivElement | undefined = $state();
  /** Whether the log keeps to its end as messages come. */
  let following = $state(true);

  let calls = $derived([...earlier, ...streamed]);

  $effect(() => {
    const path = `/runs/${encodeURIComponent(runId)}/sessions/${encodeURIComponent(session)}/stream`;
    log = NO_LOG;
    let gone = false;
    loadCalls(runId)
      .then((list) => {
        if (!gone) earlier = list.filter((call) => call.session === session);
      })
      .catch(() => {});
    const stream = follow<SessionMessage>(
      path,
      ['snapshot', 'message', 'end'],
      (message) => (log = applyLive(log, message)),
      (open) => (connection = connectionAfter(connection, open)),
      connect,
    );
    return () => {
      gone = true;
      stream.close();
    };
  });

  // New messages keep the end in view while the log follows.
  $effect(() => {
    void log.messages.length;
    if (following) void tick().then(toEnd);
  });

  function toEnd(): void {
    if (box) box.scrollTop = box.scrollHeight;
  }

  function scrolled(): void {
    if (!box) return;
    following = box.scrollHeight - box.scrollTop - box.clientHeight <= NEAR_END;
  }

  function jump(): void {
    following = true;
    toEnd();
  }
</script>

<section class="panel live-session" aria-labelledby="live-title">
  <div class="panel-head">
    <h2 id="live-title">Conversation of <span class="mono">{session}</span></h2>
    {#if !log.ended}<Live {connection} />{/if}
  </div>
  {#if log.cut}
    <p class="panel-note">Earlier turns are in the transcript once the lane ends.</p>
  {/if}
  <div class="log" bind:this={box} onscroll={scrolled} role="log" aria-live="off" tabindex="-1">
    {#if log.messages.length === 0 && !log.ended}
      <p class="muted waiting">Waiting for the first message.</p>
    {/if}
    {#each log.messages as item (item.seq)}
      <article class="message {item.message.role}">
        <header>
          <span>{item.message.role}</span>
          <span class="muted">{item.message.turn === 0 ? 'opening' : `turn ${item.message.turn}`}</span>
          <span class="muted"><Time iso={item.at} /></span>
        </header>
        <MessageParts
          parts={item.message.parts}
          calls={item.message.role === 'assistant' ? callsOf(calls, session, item.message.turn) : null}
        />
      </article>
    {/each}
  </div>
  <div class="panel-foot">
    {#if log.ended && log.elsewhere}
      <span>This lane runs in another Henk process, whose conversation this page cannot hear. Its tool calls show on the run.</span>
    {:else if log.ended}
      <span>
        The lane has ended. <a href={href(transcriptPath(runId, session))} use:link>The whole conversation</a>
        is in its transcript, where old tool results may be shortened.
      </span>
    {:else}
      <span>Shown as it happens, before old tool results are shortened for the model.</span>
    {/if}
    {#if !following}
      <button type="button" class="jump" onclick={jump}>Jump to latest</button>
    {/if}
  </div>
</section>

<style>
  .log {
    max-height: min(36rem, 70vh);
    overflow-y: auto;
    padding: var(--space-3) var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    border-top: 1px solid var(--line-soft);
  }
  .message { border-left: 3px solid var(--line); padding-left: var(--space-3); }
  .message.assistant { border-left-color: var(--accent); }
  .message header { display: flex; gap: var(--space-2); font-weight: 600; margin-bottom: var(--space-1); }
  .waiting { margin: 0; }
  .panel-note { margin: 0; padding: var(--space-2) var(--space-4); color: var(--muted); }
  .jump { margin-left: auto; }
</style>
