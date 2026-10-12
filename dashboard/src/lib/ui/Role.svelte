<script lang="ts">
  import { isHistory, roleLabel } from '$lib/roles';

  /** The role a session ran in, as a pill (#326); the roles of older runs
   * muted. */
  let { role }: { role: string } = $props();
</script>

<span class="pill role role-{role}" class:tone-accent={!isHistory(role)} class:tone-neutral={isHistory(role)}>{roleLabel(role)}</span>
