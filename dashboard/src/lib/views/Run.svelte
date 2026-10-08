<script lang="ts">
  import { ApiError, cancelRun, run as loadRun, runToolCalls, startRun } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import type { Cancelled, RunDetail, RunMessage, StartRequest, Started, ToolCall } from '$lib/api/types';
  import { now } from '$lib/clock';
  import { about, clockTime, count, draftWhat, kindText, runDuration, shortCommit, utc, verdictText } from '$lib/format';
  import { applyRun, connectionAfter, type Connection } from '$lib/live';
  import { eventPath, href, link, navigate, runPath, transcriptPath } from '$lib/router';
  import Commit from '$lib/ui/Commit.svelte';
  import Dialog from '$lib/ui/Dialog.svelte';
  import Icon from '$lib/ui/Icon.svelte';
  import Live from '$lib/ui/Live.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';
  import CallTimeline from './CallTimeline.svelte';
  import LiveSession from './LiveSession.svelte';
  import Pipeline from './Pipeline.svelte';

  /** `who` is the viewer's id (`github:1234`), which a cancel names.
   * `load`, `cancel`, `connect`, `loadCalls`, `start` and `go` are
   * replaced in the tests. */
  let {
    id,
    who = null,
    load = loadRun,
    cancel = cancelRun,
    connect = connectEventSource,
    loadCalls = runToolCalls,
    start = startRun,
    go = navigate,
  }: {
    id: string;
    who?: string | null;
    load?: (id: string) => Promise<RunDetail>;
    cancel?: (id: string) => Promise<Cancelled>;
    connect?: Connect;
    loadCalls?: (id: string) => Promise<ToolCall[]>;
    start?: (request: StartRequest) => Promise<Started>;
    go?: (path: string) => void;
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
    'stages',
    'heartbeat',
    'end',
  ];

  let d: RunDetail | null = $state(null);
  /** The calls the stream brought since the page opened, for the live
   * conversation of a lane (#238); the newest few thousand. */
  let streamedCalls: ToolCall[] = $state([]);
  const MOST_STREAMED_CALLS = 2000;
  let problem: unknown = $state(null);
  let connection: Connection = $state('connecting');
  let cancelling = $state(false);
  let cancelNote: string | null = $state(null);
  let confirmingCancel = $state(false);
  let confirmingAgain = $state(false);
  let startingAgain = $state(false);
  let againProblem: string | null = $state(null);

  // Loads the run; while it runs, follows its stream until it ends.
  $effect(() => {
    const wanted = id;
    let following: { close(): void } | null = null;
    let gone = false;
    d = null;
    streamedCalls = [];
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
              if (message.kind === 'tool_call') {
                streamedCalls = [...streamedCalls, message.data].slice(-MOST_STREAMED_CALLS);
              }
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

  /** The box selected in the pipeline: a stage, or `lane:<name>`. */
  let selected: string | null = $state(null);

  /** The lane selected in the pipeline, if a lane is. */
  let selectedLane = $derived.by(() => {
    const detail: RunDetail | null = d;
    const key: string | null = selected;
    if (detail === null || key === null || !key.startsWith('lane:')) return null;
    const name = key.slice('lane:'.length);
    return detail.lanes.find((lane) => lane.name === name) ?? null;
  });

  /** The timeline lines of what is selected: a lane's own lines, or the
   * lines written while a stage was going. */
  let shownEvents = $derived.by(() => {
    if (d === null || selected === null) return d?.events ?? [];
    const detail = d;
    if (selected.startsWith('lane:')) {
      const lane = selected.slice('lane:'.length);
      return detail.events.filter((line) => line.message.startsWith(`${lane}:`));
    }
    const stage = detail.stages.find((s) => s.name === selected);
    if (stage === undefined) return detail.events;
    const from = Date.parse(stage.started_at);
    const to = stage.ended_at === null ? Number.POSITIVE_INFINITY : Date.parse(stage.ended_at);
    return detail.events.filter((line) => {
      const at = Date.parse(line.at);
      return at >= from - 1000 && at <= to + 1000;
    });
  });

  function selectedName(key: string): string {
    return key.startsWith('lane:') ? key.slice('lane:'.length) : key.replaceAll('_', ' ');
  }

  /** How the run's stages go, in a line, by kind. */
  function pipelineNote(detail: RunDetail): string {
    if (detail.run.kind === 'review') {
      return 'Lanes review the same commit at once. The fact-check starts when every lane has ended.';
    }
    if (detail.run.kind === 'address') return 'One commit to the branch, then replies on the threads.';
    return '';
  }

  /** What a cancel does, by kind of run: the facts of `review.rs`,
   * `plan.rs`, `address.rs` and `cancel.rs` (#230). */
  function cancelEffects(detail: RunDetail): string[] {
    const you = who === null ? 'you' : `you, ${who}`;
    const where = detail.run.platform === 'gitlab' ? 'merge request' : 'pull request';
    if (detail.run.kind === 'review') {
      return [
        'All lanes stop now. Drafts still waiting are marked cancelled.',
        `On the ${where}, one comment and the check say it was cancelled and name ${you}.`,
        'Nothing already posted is removed.',
      ];
    }
    if (detail.run.kind === 'plan') {
      return ['The planner stops now.', `On the issue, one comment says it was cancelled and names ${you}.`];
    }
    if (detail.run.kind === 'address') {
      return [
        'The address run stops now. If it had not pushed yet, nothing is pushed.',
        `On the ${where}, one comment says it was cancelled and names ${you}.`,
      ];
    }
    return [`Henk stops now and says it was cancelled, naming ${you}.`];
  }

  /** The sessions still going, for the confirmation. */
  function stillWorking(detail: RunDetail): string {
    const names = detail.lanes.filter((lane) => lane.status === 'running').map((lane) => lane.name);
    if (names.length === 0) return '';
    return `${names.join(', ')} ${names.length === 1 ? 'is' : 'are'} still working.`;
  }

  /** A finished review can be started again for its commit; a running one
   * would only be joined, and a superseded one is behind a newer commit. */
  function againable(detail: RunDetail): boolean {
    const r = detail.run;
    return (
      r.kind === 'review' &&
      r.status !== 'running' &&
      r.status !== 'superseded' &&
      r.target_url !== null &&
      r.commit !== null
    );
  }

  async function reviewAgain(detail: RunDetail): Promise<void> {
    if (detail.run.target_url === null || detail.run.commit === null) return;
    startingAgain = true;
    againProblem = null;
    try {
      const started = await start({
        kind: 'review',
        url: detail.run.target_url,
        commit: detail.run.commit,
        note: null,
      });
      confirmingAgain = false;
      go(eventPath(started.event_id));
    } catch (error) {
      againProblem = error instanceof ApiError ? error.message : 'The start did not go through.';
    } finally {
      startingAgain = false;
    }
  }

  async function stop(): Promise<void> {
    confirmingCancel = false;
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
        <span class="push actions">
          {#if againable(d)}
            <button type="button" onclick={() => { againProblem = null; confirmingAgain = true; }}><Icon name="undo" />Review again</button>
          {/if}
          {#if d.run.status === 'running'}
            <button type="button" class="danger" onclick={() => (confirmingCancel = true)} disabled={cancelling}><Icon name="slash" />Cancel run</button>
          {/if}
        </span>
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
        {#if d.run.superseded_by}
          <div>
            <dt>Replaced by</dt>
            <dd><a class="mono" href={href(runPath(d.run.superseded_by))} use:link>{d.run.superseded_by}</a></dd>
          </div>
        {/if}
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

  <Dialog bind:open={confirmingCancel} title="Cancel this {kindText(d.run.kind)}?">
    <p>
      <code>{d.run.id}</code>
      {#if d.run.repo}on <strong>{about(d.run.repo, d.run.target)}</strong>{/if}
      has run for {runDuration(d.run, $now)}. {stillWorking(d)}
    </p>
    <ul class="effects">
      {#each cancelEffects(d) as line (line)}
        <li>{line}</li>
      {/each}
    </ul>
    {#snippet actions()}
      <button type="button" data-default onclick={() => (confirmingCancel = false)}>Keep running</button>
      <button type="button" class="danger-solid" onclick={stop}>Cancel {kindText(d?.run.kind ?? 'run')}</button>
    {/snippet}
  </Dialog>

  <Dialog bind:open={confirmingAgain} title="Review again">
    <p><strong>A new run, not a retry of this one.</strong></p>
    <p>
      Henk starts a new review of <code>{shortCommit(d.run.commit ?? '')}</code> on
      {about(d.run.repo, d.run.target)}. If a review of that commit is running, the request joins it
      instead. This run stays as it is.
    </p>
    {#if againProblem !== null}
      <p class="problem" role="alert"><strong>Not started.</strong> {againProblem}</p>
    {/if}
    {#snippet actions()}
      <button type="button" onclick={() => (confirmingAgain = false)}>Not now</button>
      <button type="button" class="primary" data-default disabled={startingAgain} onclick={() => d && reviewAgain(d)}>
        {startingAgain ? 'Starting' : 'Start a new review'}
      </button>
    {/snippet}
  </Dialog>

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

  {#if d.stages.length > 0}
    <section class="panel">
      <div class="panel-head">
        <h2>Pipeline</h2>
        <span class="note">{pipelineNote(d)} Select a box to open its log.</span>
      </div>
      <div class="panel-body"><Pipeline stages={d.stages} lanes={d.lanes} bind:selected /></div>
    </section>
  {/if}

  {#if selectedLane !== null && selectedLane.status === 'running'}
    {#key selectedLane.name}
      <LiveSession runId={d.run.id} session={selectedLane.name} streamed={streamedCalls} {connect} {loadCalls} />
    {/key}
  {/if}

  {#if d.findings.length > 0}
    <section class="list">
      <div class="list-head"><h2>Findings <span class="count-badge">{d.findings.length}</span></h2></div>
      <ul class="cards findings">
        {#each d.findings as finding, index (index)}
          {@const source = d.drafts.find((draft) => draft.decision?.comment_id === finding.comment_id && finding.comment_id !== '')}
          <li>
            <div class="meta">
              <span class="mono">{finding.comment_id}</span>
              <Status word={finding.action} vocabulary="action" kind="action" />
              <span class="mono muted">{finding.lane}</span>
              <code>{finding.path}:{finding.line}</code>
              <span class="muted mono" title={utc(finding.at)}>{clockTime(finding.at)}</span>
            </div>
            {#if source}
              <p class="text text-block">{draftWhat(source)}</p>
              {#if source.decision && source.decision.reason !== ''}
                <p class="reason text">{verdictText(source)} by {source.decision.checker}: {source.decision.reason}</p>
              {/if}
            {/if}
          </li>
        {/each}
      </ul>
    </section>
  {/if}

  {#if d.lanes.length > 0}
    <section class="panel fold">
      <details open={d.stages.length === 0}>
        <summary class="panel-head"><h2>Lanes</h2><span class="count-badge">{d.lanes.length}</span></summary>
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
      </details>
    </section>
  {/if}

  {#if d.drafts.length > 0}
    <section class="panel fold">
      <details open={d.stages.length === 0}>
        <summary class="panel-head"><h2>Drafts</h2><span class="count-badge">{d.drafts.length}</span></summary>
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
      </details>
    </section>
  {/if}

  {#if d.tool_usage.length > 0}
    <section class="panel fold">
      <details open={false}>
        <summary class="panel-head"><h2>Tool calls</h2><span class="count-badge">{d.tool_usage.length}</span></summary>
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
      </details>
    </section>
  {/if}

  {#if d.events.length > 0}
    <section class="panel fold">
      <details open={selected !== null || d.stages.length === 0}>
        <summary class="panel-head"><h2>Timeline</h2><span class="count-badge">{d.events.length}</span></summary>
          {#if selected !== null}
            <p class="filter-note">Showing the lines of {selectedName(selected)}: {shownEvents.length} of {d.events.length}. <button type="button" class="link" onclick={() => (selected = null)}>Show all</button></p>
          {/if}
      <div class="scroll">
        <table>
          <thead><tr><th>At</th><th>Level</th><th>What</th></tr></thead>
          <tbody>
            {#each shownEvents as line, index (index)}
              <tr>
                <td class="mono nowrap" title={utc(line.at)}>{clockTime(line.at)}</td>
                <td><Status word={line.level} kind="level" /></td>
                <td class="text">{line.message}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      </details>
    </section>
  {/if}

  {#if d.requests.length > 0}
    <section class="panel fold">
      <details open={false}>
        <summary class="panel-head"><h2>Requests</h2><span class="count-badge">{d.requests.length}</span></summary>
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
      </details>
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
  .actions { display: inline-flex; gap: var(--space-2); }
  .run-head .note { margin: var(--space-3) 0 0; }
  .label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); margin-bottom: var(--space-2); }
  .label + .text-block { margin-bottom: var(--space-2); }
  .calls-body { padding-top: var(--space-3); border-top: 1px solid var(--line-soft); }
  .fold summary {
    cursor: pointer;
    list-style: none;
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
  }
  .fold :global(.scroll) { overflow-x: auto; border-top: 1px solid var(--line-soft); }
  .fold :global(.calls-body) { padding: var(--space-3) var(--space-4) var(--space-4); }
  .fold summary::-webkit-details-marker { display: none; }
  .fold summary::before { content: '\25B8'; color: var(--muted); }
  .fold details[open] > summary::before { content: '\25BE'; }
  .filter-note { margin: 0; padding: 0 var(--space-4) var(--space-2); color: var(--muted); font-size: 13px; }
  .link { border: 0; background: none; color: var(--accent); padding: 0; min-height: 0; font-size: inherit; }
  .list { display: flex; flex-direction: column; gap: var(--space-2); }
</style>
