<script lang="ts">
  import { shortCommit } from '$lib/format';
  import Icon from './Icon.svelte';

  /** A commit: 7 characters, the full SHA on hover and a copy button. */
  let { sha, copy = (text: string) => navigator.clipboard.writeText(text) }: {
    sha: string;
    copy?: (text: string) => Promise<void>;
  } = $props();

  let copied = $state(false);

  async function copyIt(): Promise<void> {
    try {
      await copy(sha);
      copied = true;
      setTimeout(() => (copied = false), 1500);
    } catch {
      // The clipboard said no; the SHA is still in the title.
    }
  }
</script>

<span class="commit"><code title={sha}>{shortCommit(sha)}</code><button type="button" class="icon-button" onclick={copyIt} aria-label="Copy the commit {sha}" title="Copy"><Icon name={copied ? 'check' : 'copy'} size={13} /></button>{#if copied}<span class="visually-hidden" role="status">Copied.</span>{/if}</span>
