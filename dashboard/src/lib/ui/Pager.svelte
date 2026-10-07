<script lang="ts">
  import { href, link, withQuery } from '$lib/router';

  /** `path` with `query` is this page; `next` the cursor of the next. */
  let {
    path,
    query,
    next,
  }: { path: string; query: URLSearchParams; next: string | null } = $props();

  let keep = $derived(
    Object.fromEntries([...query.entries()].filter(([key]) => key !== 'cursor')),
  );
</script>

<p class="pager">
  {#if query.has('cursor')}
    <a href={href(withQuery(path, keep))} use:link>Newest</a>
  {/if}
  {#if next !== null}
    <a href={href(withQuery(path, { ...keep, cursor: next }))} use:link>Older</a>
  {/if}
</p>
