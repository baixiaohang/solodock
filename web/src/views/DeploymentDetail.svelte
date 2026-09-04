<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import type { AppDetailResponse, Deployment } from '../lib/types'
  import { isTerminalDeployment } from '../lib/deploymentState'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { formatTimestamp, timeSettings } from '../lib/time'
  import { locale, localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { stateText, transitionResultText } from '../lib/presentation'
  let { deploymentId }: { deploymentId: string } = $props()
  let deployment = $state<Deployment | null>(null)
  let timer: ReturnType<typeof setTimeout> | undefined
  let loadController: AbortController | undefined
  let loadGeneration = 0
  let transientFailures = 0
  let error = $state<UserMessage | null>(null)
  interface RollbackRequest {
    expected_active_release_id: string | null
    expected_pending_release_id: string | null
    expected_actual_release_id: string | null
    expected_actual_container_id: string | null
    acknowledge_non_rollbackable_data: boolean
  }
  interface RollbackAttempt { endpoint: string; request: RollbackRequest; retry: RetryIdentity }
  let rollbackAttempt = $state<RollbackAttempt | undefined>()
  let rollbackBusy = $state(false)
  let disposed = false
  onMount(() => {
    disposed = false
    void load()
    return () => {
      disposed = true
      loadGeneration += 1
      loadController?.abort()
      loadController = undefined
      clearScheduledLoad()
      rollbackAttempt = undefined
    }
  })

  function clearScheduledLoad() {
    if (timer) clearTimeout(timer)
    timer = undefined
  }

  function scheduleLoad(delay: number) {
    clearScheduledLoad()
    const scheduled = setTimeout(() => {
      if (timer === scheduled) timer = undefined
      void load()
    }, delay)
    timer = scheduled
  }

  async function load() {
    if (disposed || loadController) return
    clearScheduledLoad()
    const generation = ++loadGeneration
    const controller = new AbortController()
    loadController = controller
    try {
      const loaded = await api<Deployment>(`/api/v1/deployments/${deploymentId}`, { signal: controller.signal })
      if (disposed || generation !== loadGeneration) return
      deployment = loaded
      error = null
      transientFailures = 0
      if (!isTerminalDeployment(loaded.status)) scheduleLoad(1000)
    } catch {
      if (disposed || generation !== loadGeneration) return
      error = localized('Could not load deployment.')
      const delay = Math.min(1000 * (2 ** transientFailures), 15000)
      transientFailures += 1
      scheduleLoad(delay)
    } finally {
      if (generation === loadGeneration) loadController = undefined
    }
  }
  async function rollback() {
    if (disposed || rollbackBusy || !deployment || !window.confirm($t('Rollback changes only the image and configuration; database, named volume, and bind contents are not reverted. Continue?'))) return
    rollbackBusy = true
    try {
      let attempt = rollbackAttempt
      if (!attempt) {
        let app: AppDetailResponse
        try {
          app = await api<AppDetailResponse>(`/api/v1/apps/${deployment.app_id}`)
        } catch {
          if (!disposed) error = localized('Could not prepare the rollback request. Refresh the application state and try again.')
          return
        }
        if (disposed) return
        const request: RollbackRequest = {
          expected_active_release_id: app.active_release?.id ?? null,
          expected_pending_release_id: app.pending_release_id,
          expected_actual_release_id: app.actual_release_id,
          expected_actual_container_id: app.actual?.id ?? null,
          acknowledge_non_rollbackable_data: true,
        }
        attempt = {
          endpoint: `/api/v1/deployments/${deployment.id}/rollback`,
          request,
          retry: retryIdentity(undefined, request),
        }
        rollbackAttempt = attempt
      }
      try {
        const result = await mutation<{ deployment_id: string }>(attempt.endpoint, attempt.request, { idempotencyKey: attempt.retry.key })
        if (disposed) return
        rollbackAttempt = undefined
        window.location.hash = `/deployments/${result.deployment_id}`
      } catch (cause) {
        if (!disposed) {
          const failure = mutationFailure(attempt.retry, cause)
          rollbackAttempt = failure.retry ? { ...attempt, retry: failure.retry } : undefined
          error = localized(failure.outcome === 'outcome_unknown'
            ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
            : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
        }
      }
    } finally {
      if (!disposed) rollbackBusy = false
    }
  }
</script>
<main class="page-shell narrow">
  <a class="back" href={deployment ? `#/apps/${deployment.app_id}` : '#/'}>← {$t('Back to application')}</a>
  {#if error}<p class="notice danger" role="alert">{messageText(error, $t)}</p>{/if}
  {#if deployment}
    <div class="page-heading"><div><p class="eyebrow">{$t('DEPLOYMENT')}</p><h1>{stateText(deployment.status, $t)}</h1><code>{deployment.id}</code></div><span class="state-pill large">{stateText(deployment.phase, $t)}</span></div>
    <article class="panel"><dl class="fact-list"><div><dt>{$t('Trigger')}</dt><dd>{stateText(deployment.trigger, $t)}</dd></div><div><dt>{$t('Source tag')}</dt><dd><code>{deployment.source_image_ref ?? '—'}</code></dd></div><div><dt>{$t('Stop grace period')}</dt><dd>{deployment.candidate_stop_grace_period_seconds === null || deployment.candidate_stop_grace_period_seconds === undefined ? '—' : $t('{seconds} seconds', { seconds: deployment.candidate_stop_grace_period_seconds })}</dd></div><div><dt>{$t('Manifest')}</dt><dd><code>{deployment.manifest_digest ?? '—'}</code></dd></div><div><dt>{$t('Platform')}</dt><dd>{deployment.platform ?? '—'}</dd></div><div><dt>{$t('Error')}</dt><dd><code>{deployment.error_code ?? '—'}</code></dd></div></dl></article>
    {#if deployment.warnings?.length}<p class="notice warning">{deployment.warnings.join(' · ')}</p>{/if}
    <article class="panel"><h2>{$t('Current facts')}</h2><dl class="fact-list"><div><dt>{$t('Safe release')}</dt><dd><code>{deployment.safe_release_id ?? '—'}</code></dd></div><div><dt>{$t('Active')}</dt><dd><code>{deployment.current_active_release_id ?? '—'}</code></dd></div><div><dt>{$t('Pending')}</dt><dd><code>{deployment.current_pending_release_id ?? '—'}</code></dd></div><div><dt>{$t('Actual')}</dt><dd><code>{deployment.current_actual_release_id ?? '—'}</code></dd></div></dl></article>
    <article class="panel"><h2>{$t('Timeline')}</h2><ol class="timeline">{#each deployment.transitions ?? [] as item}<li><code>{item.seq}</code><span>{stateText(item.phase, $t)} · {transitionResultText(item.result, $t)} {item.code ?? ''}<small><time datetime={item.created_at}>{formatTimestamp(item.created_at, $timeSettings.timezone, $locale)}</time></small></span></li>{/each}</ol></article>
    {#if deployment.available_actions.includes('rollback')}<button class="ghost" disabled={rollbackBusy} onclick={() => void rollback()}>{$t('Roll back to this release…')}</button>{/if}
  {/if}
</main>
