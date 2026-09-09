<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation, ApiError } from '../lib/api'
  import { t, locale } from '../lib/i18n'
  import { formatTimestamp, timeSettings } from '../lib/time'
  import type { RetentionPolicy } from '../lib/types'
  let { appId }: { appId: string } = $props()
  let policy = $state<RetentionPolicy | null>(null)
  let enabled = $state(false)
  let keep = $state<number | undefined>(3)
  let busy = $state(false)
  let failed = $state(false)
  let dirty = $state(false)
  let retry = $state<{ key: string; body: { expected_revision: string; enabled: boolean; keep_versions: number } } | null>(null)
  let disposed = false
  let loading = false
  let generation = 0
  const controller = new AbortController()
  async function refresh() {
    if (loading || busy || disposed) return
    loading = true
    const requestedGeneration = generation
    try {
      const value = await api<RetentionPolicy>(`/api/v1/apps/${appId}/retention`, { signal: controller.signal })
      if (disposed || busy || requestedGeneration !== generation) return
      if (!dirty && !retry) { policy = value; enabled = value.enabled; keep = value.keep_versions }
      else if (policy) { policy = { ...policy, last_status: value.last_status, last_checked_at: value.last_checked_at, last_error_code: value.last_error_code } }
    } catch { if (!disposed) failed = true } finally { loading = false }
  }
  onMount(() => {
    void refresh()
    const timer = setInterval(() => { void refresh() }, 15000)
    return () => { disposed = true; controller.abort(); clearInterval(timer) }
  })
  async function save() {
    if (busy || !policy || !keep || !Number.isInteger(keep) || keep < 1 || keep > 100) return
    const request = retry ?? { key: crypto.randomUUID(), body: { expected_revision: policy.revision, enabled, keep_versions: keep } }
    generation++; busy = true; failed = false
    try {
      const value = await mutation<RetentionPolicy>(`/api/v1/apps/${appId}/retention`, request.body, { method: 'PUT', idempotencyKey: request.key })
      if (!disposed) { policy = value; enabled = value.enabled; keep = value.keep_versions; dirty = false; retry = null }
    } catch (error) {
      if (!disposed) {
        failed = true
        retry = error instanceof ApiError && error.mutationOutcome === 'known_not_applied' ? null : request
      }
    } finally { busy = false }
  }
</script>

<article class="panel wide">
  <h2>{$t('Automatic version cleanup')}</h2>
  <p class="muted">{$t('Keep the active version and the most recently successful versions. Pending and recovery versions are protected in addition. Enabling also cleans existing old releases and their unused local images.')}</p>
  {#if policy}
    <form onsubmit={(event) => { event.preventDefault(); void save() }}>
      <label class="checkbox"><input type="checkbox" bind:checked={enabled} onchange={() => dirty = true} disabled={busy || !!retry} /> {$t('Automatically clean old versions')}</label>
      <label>{$t('Versions to keep (1–100)')}<input type="number" min="1" max="100" step="1" required bind:value={keep} oninput={() => dirty = true} disabled={busy || !!retry} /></label>
      <p>{$t('Status')}: {policy.enabled ? $t('Enabled') : $t('Disabled')} · {$t('Last result')}: {policy.last_status === 'blocked' ? $t('Cleanup blocked; automatic retry is scheduled.') : policy.last_status === 'partially_retained' ? $t('Some artifacts or images remain protected.') : policy.last_status === 'completed' ? $t('Cleanup pass completed.') : $t('Waiting for cleanup.')}</p>
      {#if policy.last_error_code}<p class="notice warning">{policy.last_error_code === 'APP_BUSY' ? $t('An application is busy. Cleanup will retry.') : policy.last_error_code === 'IMAGE_CLEANUP_INVENTORY_INCOMPLETE' ? $t('Release cleanup finished, but safe image cleanup could not be confirmed. Images are retained until a safe retry.') : $t('Release cleanup is blocked by incomplete recovery facts or a storage error.')}</p>{/if}
      <p>{$t('Last checked')}: {formatTimestamp(policy.last_checked_at, $timeSettings.timezone, $locale)}</p>
      <button disabled={busy}>{retry ? $t('Retry the same policy change') : $t('Save cleanup policy')}</button>
    </form>
  {/if}
  {#if failed}<p role="alert">{$t('Could not confirm the cleanup policy request. Retry or refresh the policy before editing again.')}</p><button class="ghost" disabled={busy || !!retry} onclick={() => { dirty = false; failed = false; void refresh() }}>{$t('Refresh policy')}</button>{/if}
  <p><a href="#/settings">{$t('Preview manual storage and image cleanup')}</a></p>
  <p class="muted">{$t('Cleanup preserves deployment history and workload data. Shared image layers may remain, so reported size is not guaranteed reclaimed space.')}</p>
</article>
