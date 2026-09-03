<script lang="ts">
  import { t } from '../lib/i18n'
  let { configured }: { configured: boolean } = $props()
</script>

{#if configured}
  <p class="notice danger" role="alert">
    {$t('This application\'s Registry webhook secret will be permanently deleted when it is unregistered. Existing senders will stop working immediately.')}
  </p>
{/if}
