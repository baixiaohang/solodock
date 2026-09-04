<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import type { AppDetailResponse, Deployment } from '../lib/types'
  import { isTerminalDeployment } from '../lib/deploymentState'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { formatTimestamp, timeSettings } from '../lib/time'
  import { locale, localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { stateText, transitionResultText } from '../lib/presentation'
  let { deploymentId }: { deploymentId: string } = $props()
  let deployment = $state<Deployment | null>(null)
  let timer: ReturnType<typeof setTimeout> | undefined
  let error = $state<UserMessage | null>(null)
  let rollbackRetry = $state<RetryIdentity | undefined>()
  let disposed = false
  onMount(() => {
    disposed = false
    void load()
    return () => {
      disposed = true
      if (timer) clearTimeout(timer)
      timer = undefined
      rollbackRetry = undefined
    }
  })
  async function load() {
    if (disposed) return
    try {
      const loaded = await api<Deployment>(`/api/v1/deployments/${deploymentId}`)
      if (disposed) return
      deployment = loaded
      if (deployment && !isTerminalDeployment(deployment.status)) timer = setTimeout(() => void load(), 1000)
    } catch { if (!disposed) error = localized('Could not load deployment.') }
  }
  async function rollback() {
    if (disposed || !deployment || !window.confirm($t('Rollback changes only the image and configuration; database, named volume, and bind contents are not reverted. Continue?'))) return
    try {
      const app = await api<AppDetailResponse>(`/api/v1/apps/${deployment.app_id}`)
      if (disposed) return
      const request = {
        expected_active_release_id: app.active_release?.id ?? null,
        expected_pending_release_id: app.pending_release_id,
        expected_actual_release_id: app.actual_release_id,
        expected_actual_container_id: app.actual?.id ?? null,
        acknowledge_non_rollbackable_data: true,
      }
      rollbackRetry = retryIdentity(rollbackRetry, request)
      const result = await mutation<{ deployment_id: string }>(`/api/v1/deployments/${deployment.id}/rollback`, request, { idempotencyKey: rollbackRetry.key })
      if (disposed) return
      rollbackRetry = undefined
      window.location.hash = `/deployments/${result.deployment_id}`
    } catch { if (!disposed) error = localized('Rollback facts changed. Return to the application page and refresh.') }
  }
</script>
<main class="page-shell narrow">
  <a class="back" href={deployment ? `#/apps/${deployment.app_id}` : '#/'}>← {$t('Back to application')}</a>
  {#if error}<p class="notice danger">{messageText(error, $t)}</p>{/if}
  {#if deployment}
    <div class="page-heading"><div><p class="eyebrow">{$t('DEPLOYMENT')}</p><h1>{stateText(deployment.status, $t)}</h1><code>{deployment.id}</code></div><span class="state-pill large">{stateText(deployment.phase, $t)}</span></div>
    <article class="panel"><dl class="fact-list"><div><dt>{$t('Trigger')}</dt><dd>{stateText(deployment.trigger, $t)}</dd></div><div><dt>{$t('Source tag')}</dt><dd><code>{deployment.source_image_ref ?? '—'}</code></dd></div><div><dt>{$t('Stop grace period')}</dt><dd>{deployment.candidate_stop_grace_period_seconds === null || deployment.candidate_stop_grace_period_seconds === undefined ? '—' : $t('{seconds} seconds', { seconds: deployment.candidate_stop_grace_period_seconds })}</dd></div><div><dt>{$t('Manifest')}</dt><dd><code>{deployment.manifest_digest ?? '—'}</code></dd></div><div><dt>{$t('Platform')}</dt><dd>{deployment.platform ?? '—'}</dd></div><div><dt>{$t('Error')}</dt><dd><code>{deployment.error_code ?? '—'}</code></dd></div></dl></article>
    {#if deployment.warnings?.length}<p class="notice warning">{deployment.warnings.join(' · ')}</p>{/if}
    <article class="panel"><h2>{$t('Current facts')}</h2><dl class="fact-list"><div><dt>{$t('Safe release')}</dt><dd><code>{deployment.safe_release_id ?? '—'}</code></dd></div><div><dt>{$t('Active')}</dt><dd><code>{deployment.current_active_release_id ?? '—'}</code></dd></div><div><dt>{$t('Pending')}</dt><dd><code>{deployment.current_pending_release_id ?? '—'}</code></dd></div><div><dt>{$t('Actual')}</dt><dd><code>{deployment.current_actual_release_id ?? '—'}</code></dd></div></dl></article>
    <article class="panel"><h2>{$t('Timeline')}</h2><ol class="timeline">{#each deployment.transitions ?? [] as item}<li><code>{item.seq}</code><span>{stateText(item.phase, $t)} · {transitionResultText(item.result, $t)} {item.code ?? ''}<small><time datetime={item.created_at}>{formatTimestamp(item.created_at, $timeSettings.timezone, $locale)}</time></small></span></li>{/each}</ol></article>
    {#if deployment.available_actions.includes('rollback')}<button class="ghost" onclick={() => void rollback()}>{$t('Roll back to this release…')}</button>{/if}
  {/if}
</main>
