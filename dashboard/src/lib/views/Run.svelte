<script lang="ts">
  import { ApiError, cancelRun, run as loadRun } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import type { Cancelled, RunDetail, RunMessage } from '$lib/api/types';
  import { about, draftWhat, kindText, verdictText } from '$lib/format';
  import { applyRun } from '$lib/live';
  import { eventPath, href, link, transcriptPath } from '$lib/router';
  import Problem from './Problem.svelte';

  /** `load`, `cancel` and `connect` are replaced in the tests. */
  let {
    id,
    load = loadRun,
    cancel = cancelRun,
    connect = connectEventSource,
  }: {
    id: string;
    load?: (id: string) => Promise<RunDetail>;
    cancel?: (id: string) => Promise<Cancelled>;
    connect?: Connect;
  } = $props();

  const KINDS: RunMessage['kind'][] = [
    'snapshot',
    'run',
    'lanes',
    'tool_call',
    'draft',
    'finding',
    'event',
    'transcript',
    'end',
  ];

  let d: RunDetail | null = $state(null);
  let problem: unknown = $state(null);
  let live = $state(false);
  let cancelling = $state(false);
  let cancelNote: string | null = $state(null);

  // Loads the run; while it runs, follows its stream until it ends.
  $effect(() => {
    const wanted = id;
    let following: { close(): void } | null = null;
    let gone = false;
    d = null;
    problem = null;
    live = false;
    load(wanted)
      .then((detail) => {
        if (gone) return;
        d = detail;
        if (detail.run.status === 'running') {
          following = follow<RunMessage>(
            `/runs/${encodeURIComponent(wanted)}/stream`,
            KINDS,
            (message) => {
              if (d !== null) d = applyRun(d, message);
            },
            (on) => (live = on),
            connect,
          );
        }
      })
      .catch((error: unknown) => {
        if (!gone) problem = error;
      });
    return () => {
      gone = true;
      following?.close();
    };
  });

  async function stop(): Promise<void> {
    cancelling = true;
    cancelNote = null;
    try {
      await cancel(id);
      cancelNote = 'Cancel sent. The run ends as cancelled.';
    } catch (error) {
      cancelNote = error instanceof ApiError ? error.message : 'The cancel did not go through.';
    } finally {
      cancelling = false;
    }
  }
</script>

{#if problem !== null}
  <Problem error={problem} />
{:else if d === null}
  <p class="muted" aria-busy="true">Loading.</p>
{:else}
  <h1>{kindText(d.run.kind)} {d.run.id}{#if live && d.run.status === 'running'}<span class="badge" title="Updates as it happens">live</span>{/if}</h1>
  <dl class="facts">
    <dt>About</dt>
    <dd>
      {d.run.platform}
      {#if d.run.target_url}
        <a href={d.run.target_url} rel="noreferrer">{about(d.run.repo, d.run.target)}</a>
      {:else}
        {about(d.run.repo, d.run.target)}
      {/if}
      {#if d.run.commit}at <code>{d.run.commit}</code>{/if}
    </dd>
    <dt>Status</dt>
    <dd><span class="status {d.run.status}">{d.run.status}</span></dd>
    <dt>Started</dt>
    <dd>{d.run.started_at}{#if d.run.finished_at}, finished {d.run.finished_at}{/if}</dd>
    <dt>Trigger</dt>
    <dd>{d.run.requester ? `${d.run.trigger} (asked by ${d.run.requester})` : d.run.trigger}</dd>
    {#if d.check_id}
      <dt>Check</dt>
      <dd><code>{d.check_id}</code></dd>
    {/if}
  </dl>
  {#if d.run.status === 'running'}
    <p><button onclick={stop} disabled={cancelling}>Cancel this run</button></p>
  {/if}
  {#if cancelNote !== null}
    <p class="note" role="status">{cancelNote}</p>
  {/if}
  {#if d.summary}
    <p><strong>Summary:</strong> {d.summary}</p>
  {/if}
  {#if d.error}
    <p><strong>Error:</strong> <code>{d.error}</code></p>
  {/if}

  {#if d.requests.length > 0}
    <h2>Events</h2>
    <table>
      <thead><tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th></tr></thead>
      <tbody>
        {#each d.requests as request (request.id)}
          <tr>
            <td><a href={href(eventPath(request.id))} use:link>{request.id}</a></td>
            <td>{request.received_at}</td>
            <td>{request.source}</td>
            <td>{request.kind}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if d.lanes.length > 0}
    <h2>Lanes</h2>
    <table>
      <thead>
        <tr><th>Lane</th><th>Model</th><th>Status</th><th>Turns</th><th>Tokens in</th><th>Tokens out</th><th>Error</th></tr>
      </thead>
      <tbody>
        {#each d.lanes as lane (lane.name)}
          <tr>
            <td>
              {#if d.transcripts.some((t) => t.session === lane.name)}
                <a href={href(transcriptPath(d.run.id, lane.name))} use:link title="The whole conversation">{lane.name}</a>
              {:else}
                {lane.name}
              {/if}
            </td>
            <td>{lane.model}</td>
            <td>{lane.status}</td>
            <td>
              {#if lane.status === 'running' && lane.last_call_turn !== null}
                turn {lane.last_call_turn}
              {:else}
                {lane.turns}
              {/if}
            </td>
            <td>{lane.input_tokens}</td>
            <td>{lane.output_tokens}</td>
            <td>{lane.error ?? ''}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if d.tool_usage.length > 0}
    <h2>Tool calls</h2>
    <table>
      <thead>
        <tr><th>Session</th><th>Tool</th><th>Calls</th><th>Errors</th><th>Refused</th><th>Not run</th><th>Time (ms)</th></tr>
      </thead>
      <tbody>
        {#each d.tool_usage as row (`${row.session}/${row.tool}`)}
          <tr>
            <td>{row.session}</td>
            <td><code>{row.tool}</code></td>
            <td>{row.calls}</td>
            <td>{row.errors}</td>
            <td>{row.refusals}</td>
            <td>{row.other}</td>
            <td>{row.total_ms}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if d.drafts.length > 0}
    <h2>Drafts</h2>
    <table>
      <thead>
        <tr><th>Draft</th><th>Lane</th><th>Where</th><th>What</th><th>Verdict</th><th>By</th><th>Comment</th><th>Reason</th></tr>
      </thead>
      <tbody>
        {#each d.drafts as draft (draft.id)}
          <tr>
            <td>{draft.id}</td>
            <td>{draft.lane}</td>
            <td><code>{draft.path}:{draft.line}</code></td>
            <td class="text">{draftWhat(draft)}</td>
            <td>{verdictText(draft)}</td>
            <td>{draft.decision?.checker ?? ''}</td>
            <td>{draft.decision?.comment_id ?? ''}</td>
            <td class="text">{draft.decision?.reason ?? ''}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if d.findings.length > 0}
    <h2>Findings</h2>
    <table>
      <thead><tr><th>At</th><th>Lane</th><th>Where</th><th>Comment</th><th>What</th></tr></thead>
      <tbody>
        {#each d.findings as finding, index (index)}
          <tr>
            <td>{finding.at}</td>
            <td>{finding.lane}</td>
            <td><code>{finding.path}:{finding.line}</code></td>
            <td>{finding.comment_id}</td>
            <td>{finding.action}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  {#if d.events.length > 0}
    <h2>Timeline</h2>
    <table>
      <thead><tr><th>At</th><th>Level</th><th>What</th></tr></thead>
      <tbody>
        {#each d.events as line, index (index)}
          <tr>
            <td>{line.at}</td>
            <td><span class="level {line.level}">{line.level}</span></td>
            <td class="text">{line.message}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
{/if}
