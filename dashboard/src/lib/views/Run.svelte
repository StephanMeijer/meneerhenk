<script lang="ts">
  import { ApiError, cancelRun, run as loadRun, runToolCalls } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import type { Cancelled, RunDetail, RunMessage, ToolCall } from '$lib/api/types';
  import { now } from '$lib/clock';
  import { about, clockTime, count, draftWhat, kindText, runDuration, utc, verdictText } from '$lib/format';
  import { applyRun, connectionAfter, type Connection } from '$lib/live';
  import { eventPath, href, link, transcriptPath } from '$lib/router';
  import Commit from '$lib/ui/Commit.svelte';
  import Live from '$lib/ui/Live.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';
  import CallTimeline from './CallTimeline.svelte';

  /** `load`, `cancel` and `connect` are replaced in the tests. */
  let {
    id,
    load = loadRun,
    cancel = cancelRun,
    connect = connectEventSource,
    loadCalls = runToolCalls,
  }: {
    id: string;
    load?: (id: string) => Promise<RunDetail>;
    cancel?: (id: string) => Promise<Cancelled>;
    connect?: Connect;
    loadCalls?: (id: string) => Promise<ToolCall[]>;
  } = $props();

  /** Every call of the run, once asked for (#203). */
  let calls: Promise<ToolCall[]> | null = $state(null);

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
  let connection: Connection = $state('connecting');
  let cancelling = $state(false);
  let cancelNote: string | null = $state(null);

  // Loads the run; while it runs, follows its stream until it ends.
  $effect(() => {
    const wanted = id;
    let following: { close(): void } | null = null;
    let gone = false;
    d = null;
    problem = null;
    connection = 'connecting';
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
            (open) => (connection = connectionAfter(connection, open)),
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
  <Loading />
{:else}
  <div class="page-head">
    <span class="crumbs"><a href={href('/')} use:link>Runs</a> / <code>{d.run.id}</code></span>
  </div>
  <section class="panel run-head">
    <div class="panel-body">
      <div class="title-row">
        <h1>{kindText(d.run.kind)} <span class="mono id">{d.run.id}</span></h1>
        <Status word={d.run.status} />
        <Live connection={d.run.status === 'running' ? connection : 'ended'} />
        {#if d.run.status === 'running'}
          <button class="danger push" onclick={stop} disabled={cancelling}>Cancel this run</button>
        {/if}
      </div>
      <dl class="facts">
        <div>
          <dt>About</dt>
          <dd>
            {d.run.platform}
            {#if d.run.target_url}
              <a href={d.run.target_url} rel="noreferrer">{about(d.run.repo, d.run.target)}</a>
            {:else}
              {about(d.run.repo, d.run.target)}
            {/if}
            {#if d.run.commit}at <Commit sha={d.run.commit} />{/if}
          </dd>
        </div>
        <div>
          <dt>Started</dt>
          <dd><Time iso={d.run.started_at} /> <span class="muted mono">{clockTime(d.run.started_at)}</span></dd>
        </div>
        <div>
          <dt>Duration</dt>
          <dd>{runDuration(d.run, $now)}{#if d.run.finished_at}<span class="muted">, ended <Time iso={d.run.finished_at} /></span>{/if}</dd>
        </div>
        <div>
          <dt>Trigger</dt>
          <dd>{d.run.requester ? `${d.run.trigger} (asked by ${d.run.requester})` : d.run.trigger}</dd>
        </div>
        {#if d.check_id}
          <div>
            <dt>Check</dt>
            <dd><code>{d.check_id}</code></dd>
          </div>
        {/if}
        {#if d.run.status === 'running' && d.heartbeat_at}
          <div>
            <dt>Heartbeat</dt>
            <dd><Time iso={d.heartbeat_at} /></dd>
          </div>
        {/if}
      </dl>
      {#if cancelNote !== null}
        <p class="note" role="status">{cancelNote}</p>
      {/if}
    </div>
  </section>

  {#if d.summary || d.error}
    <section class="panel">
      <div class="panel-body">
        {#if d.summary}
          <h2 class="label">Summary</h2>
          <p class="text-block">{d.summary}</p>
          <p class="note">Henk is advisory. Nothing here approves, blocks or merges.</p>
        {/if}
        {#if d.error}
          <h2 class="label">Error</h2>
          <pre>{d.error}</pre>
        {/if}
      </div>
    </section>
  {/if}

  {#if d.lanes.length > 0}
    <section class="panel">
      <div class="panel-head"><h2>Lanes</h2></div>
      <div class="scroll">
        <table>
          <thead>
            <tr>
              <th>Lane</th><th>Model</th><th>Status</th><th class="num">Turns</th><th class="num">Tokens in</th>
              <th class="num">Tokens out</th><th>Error</th>
            </tr>
          </thead>
          <tbody>
            {#each d.lanes as lane (lane.name)}
              <tr>
                <td class="mono">
                  {#if d.transcripts.some((t) => t.session === lane.name)}
                    <a href={href(transcriptPath(d.run.id, lane.name))} use:link title="The whole conversation">{lane.name}</a>
                  {:else}
                    {lane.name}
                  {/if}
                </td>
                <td class="mono">{lane.model}</td>
                <td><Status word={lane.status} kind="lane-status" /></td>
                <td class="num">
                  {#if lane.status === 'running' && lane.last_call_turn !== null}
                    turn {lane.last_call_turn}
                  {:else}
                    {lane.turns}
                  {/if}
                </td>
                <td class="num">{count(lane.input_tokens)}</td>
                <td class="num">{count(lane.output_tokens)}</td>
                <td class="text">{lane.error ?? ''}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}

  {#if d.findings.length > 0}
    <section class="panel">
      <div class="panel-head"><h2>Findings</h2></div>
      <div class="scroll">
        <table>
          <thead><tr><th>At</th><th>Lane</th><th>Where</th><th>Comment</th><th>What</th></tr></thead>
          <tbody>
            {#each d.findings as finding, index (index)}
              <tr>
                <td class="mono nowrap" title={utc(finding.at)}>{clockTime(finding.at)}</td>
                <td class="mono">{finding.lane}</td>
                <td><code>{finding.path}:{finding.line}</code></td>
                <td class="mono">{finding.comment_id}</td>
                <td><Status word={finding.action} vocabulary="action" kind="action" /></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}

  {#if d.drafts.length > 0}
    <section class="panel">
      <div class="panel-head"><h2>Drafts</h2></div>
      <div class="scroll">
        <table>
          <thead>
            <tr><th>Draft</th><th>Lane</th><th>Where</th><th>What</th><th>Verdict</th><th>By</th><th>Comment</th><th>Reason</th></tr>
          </thead>
          <tbody>
            {#each d.drafts as draft (draft.id)}
              <tr>
                <td class="mono">{draft.id}</td>
                <td class="mono">{draft.lane}</td>
                <td><code>{draft.path}:{draft.line}</code></td>
                <td class="text">{draftWhat(draft)}</td>
                <td><Status word={draft.decision?.verdict ?? 'waiting'} vocabulary="verdict" kind="verdict" text={verdictText(draft)} /></td>
                <td class="mono">{draft.decision?.checker ?? ''}</td>
                <td class="mono">{draft.decision?.comment_id ?? ''}</td>
                <td class="text">{draft.decision?.reason ?? ''}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}

  {#if d.tool_usage.length > 0}
    <section class="panel">
      <div class="panel-head"><h2>Tool calls</h2></div>
      <div class="scroll">
        <table>
          <thead>
            <tr>
              <th>Session</th><th>Tool</th><th class="num">Calls</th><th class="num">Errors</th><th class="num">Refused</th>
              <th class="num">Not run</th><th class="num">Time (ms)</th>
            </tr>
          </thead>
          <tbody>
            {#each d.tool_usage as row (`${row.session}/${row.tool}`)}
              <tr>
                <td class="mono">{row.session}</td>
                <td><code>{row.tool}</code></td>
                <td class="num">{count(row.calls)}</td>
                <td class="num">{count(row.errors)}</td>
                <td class="num">{count(row.refusals)}</td>
                <td class="num">{count(row.other)}</td>
                <td class="num">{count(row.total_ms)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      <div class="panel-body calls-body">
        {#if calls === null}
          <button type="button" onclick={() => (calls = loadCalls(id))}>Show every call</button>
        {:else}
          {#await calls}
            <Loading />
          {:then list}
            <CallTimeline runId={d.run.id} calls={list} kept={d.transcripts.map((t) => t.session)} />
          {:catch error}
            <Problem {error} retry={() => (calls = loadCalls(id))} />
          {/await}
        {/if}
      </div>
    </section>
  {/if}

  {#if d.events.length > 0}
    <section class="panel">
      <div class="panel-head"><h2>Timeline</h2></div>
      <div class="scroll">
        <table>
          <thead><tr><th>At</th><th>Level</th><th>What</th></tr></thead>
          <tbody>
            {#each d.events as line, index (index)}
              <tr>
                <td class="mono nowrap" title={utc(line.at)}>{clockTime(line.at)}</td>
                <td><Status word={line.level} kind="level" /></td>
                <td class="text">{line.message}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}

  {#if d.requests.length > 0}
    <section class="panel">
      <div class="panel-head"><h2>Events</h2></div>
      <div class="scroll">
        <table>
          <thead><tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th></tr></thead>
          <tbody>
            {#each d.requests as request (request.id)}
              <tr>
                <td><a class="mono" href={href(eventPath(request.id))} use:link>{request.id}</a></td>
                <td class="nowrap">{#if request.received_at}<Time iso={request.received_at} />{/if}</td>
                <td class="mono">{request.source}</td>
                <td class="mono">{request.kind}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    </section>
  {/if}
{/if}

<style>
  .title-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2) var(--space-3);
    margin-bottom: var(--space-4);
  }
  .title-row .id { font-size: 22px; font-weight: 500; }
  .push { margin-left: auto; }
  .run-head .note { margin: var(--space-3) 0 0; }
  .label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); margin-bottom: var(--space-2); }
  .label + .text-block { margin-bottom: var(--space-2); }
  .calls-body { padding-top: var(--space-3); border-top: 1px solid var(--line-soft); }
</style>
