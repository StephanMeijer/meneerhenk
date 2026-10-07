<script lang="ts">
  import { lookOf, type Vocabulary } from '$lib/status';
  import Icon from './Icon.svelte';

  /** A status as a pill: its icon and its word, in its tone (#224).
   * `kind` is the extra class views and tests find it by (`status`,
   * `outcome`, `verdict`, `level`, `state`, `action`); `text` replaces the
   * word when there is more to say (`same as d1`). */
  let {
    word,
    vocabulary = 'state',
    kind = 'status',
    text,
  }: { word: string; vocabulary?: Vocabulary; kind?: string; text?: string } = $props();

  let shown = $derived(lookOf(word, vocabulary));
</script>

<span class="pill tone-{shown.tone} {kind} {word}"><Icon name={shown.icon} size={13} /><span>{text ?? shown.label}</span></span>
