<script lang="ts">
  import type { Snippet } from 'svelte';

  /** A modal dialog (#230) on the browser's own `<dialog>`: it keeps
   * focus inside, closes on Escape, and gives focus back to what opened
   * it. The element marked `data-default` gets focus first. */
  let {
    open = $bindable(false),
    title,
    children,
    actions,
  }: {
    open?: boolean;
    title: string;
    children: Snippet;
    actions?: Snippet;
  } = $props();

  let node: HTMLDialogElement | undefined = $state();
  let opener: Element | null = null;
  const id = `dialog-${Math.random().toString(36).slice(2, 10)}`;

  function isShown(dialog: HTMLDialogElement): boolean {
    return dialog.hasAttribute('open');
  }

  $effect(() => {
    const dialog = node;
    if (dialog === undefined) return;
    if (open && !isShown(dialog)) {
      opener = document.activeElement;
      // jsdom has no showModal; the attribute is what it understands.
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
      dialog.querySelector<HTMLElement>('[data-default]')?.focus();
    } else if (!open && isShown(dialog)) {
      if (typeof dialog.close === 'function') dialog.close();
      else dialog.removeAttribute('open');
      if (opener instanceof HTMLElement) opener.focus();
      opener = null;
    }
  });

  function onkeydown(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      event.preventDefault();
      open = false;
    }
  }
</script>

<dialog bind:this={node} class="dialog" aria-labelledby={id} onclose={() => (open = false)} {onkeydown}>
  {#if open}
    <div class="dialog-head">
      <h2 {id}>{title}</h2>
      <button type="button" class="icon-button close" aria-label="Close" onclick={() => (open = false)}>&times;</button>
    </div>
    <div class="dialog-body">{@render children()}</div>
    {#if actions}
      <div class="dialog-actions">{@render actions()}</div>
    {/if}
  {/if}
</dialog>
