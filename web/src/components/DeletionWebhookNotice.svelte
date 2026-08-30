<script lang="ts">
  let { configured }: { configured: boolean } = $props()
</script>

{#if configured}
  <p class="notice danger" role="alert">
    此应用的 Registry webhook secret 会随取消登记永久删除，现有发送方将立即失效。
  </p>
{/if}
