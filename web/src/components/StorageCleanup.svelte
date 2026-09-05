<script lang="ts">
  import { onDestroy } from 'svelte'
  import { ApiError, mutation } from '../lib/api'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import type { CleanupApplyResult, CleanupPreview } from '../lib/types'
  import type { CleanupProtectionReason } from '../lib/types'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'

  let preview = $state<CleanupPreview | null>(null)
  let result = $state<CleanupApplyResult | null>(null)
  let acknowledge = $state(false)
  let scanning = $state(false)
  let applying = $state(false)
  let error = $state<UserMessage | null>(null)
  let requestId = $state('')
  let applyRetry = $state<RetryIdentity | undefined>()
  let applyBody = $state<{ confirmation_token: string; acknowledge_rollback_loss: true } | undefined>()
  let generation = 0

  onDestroy(() => { generation += 1 })

  async function scan() {
    if (scanning || applying || applyRetry) return
    const current = ++generation
    scanning = true
    error = null
    requestId = ''
    result = null
    try {
      const next = await mutation<CleanupPreview>('/api/v1/system/storage-cleanup/preview', {})
      if (current !== generation) return
      preview = next
      acknowledge = false
      applyRetry = undefined
      applyBody = undefined
    } catch (cause) {
      if (current !== generation) return
      preview = null
      error = localized('Could not create a safe storage cleanup preview.')
      if (cause instanceof ApiError) requestId = cause.body.request_id
    } finally {
      if (current === generation) scanning = false
    }
  }

  async function applyCleanup() {
    if (!preview || !acknowledge || applying || scanning) return
    const current = ++generation
    applying = true
    error = null
    requestId = ''
    const request = applyBody ?? {
      confirmation_token: preview.confirmation_token,
      acknowledge_rollback_loss: true as const,
    }
    applyBody = request
    applyRetry = retryIdentity(applyRetry, request)
    try {
      const completed = await mutation<unknown>(
        '/api/v1/system/storage-cleanup/apply',
        request,
        { idempotencyKey: applyRetry.key, expectedStatus: 200 },
      )
      if (current !== generation) return
      if (!confirmedResult(completed, preview)) {
        throw new ApiError(200, { code: 'HTTP_ERROR', message: 'Unconfirmed cleanup result', request_id: '' })
      }
      result = completed
      preview = null
      acknowledge = false
      applyRetry = undefined
      applyBody = undefined
    } catch (cause) {
      if (current !== generation) return
      const failure = mutationFailure(applyRetry, cause)
      applyRetry = failure.retry
      if (cause instanceof ApiError) requestId = cause.body.request_id
      if (failure.outcome === 'outcome_unknown') {
        error = localized('The cleanup result could not be confirmed. Retry only to confirm this exact operation.')
      } else {
        preview = null
        acknowledge = false
        applyBody = undefined
        error = localized('The cleanup preview expired or storage facts changed. Scan again before applying.')
      }
    } finally {
      if (current === generation) applying = false
    }
  }

  function confirmedResult(value: unknown, expected: CleanupPreview): value is CleanupApplyResult {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
    const response = value as Record<string, unknown>
    if (typeof response.operation_id !== 'string'
      || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(response.operation_id)
      || typeof response.plan_hash !== 'string' || !/^[0-9a-f]{64}$/.test(response.plan_hash)
      || typeof response.idempotency_replayed !== 'boolean'
      || !['completed', 'completed_with_failures'].includes(String(response.status))
      || !Array.isArray(response.items) || response.items.length !== expected.candidates.length) return false
    let retained = false
    const valid = response.items.every((item: unknown, index: number) => {
      if (typeof item !== 'object' || item === null || Array.isArray(item)) return false
      const result = item as Record<string, unknown>
      const candidate = expected.candidates[index]
      if ((result.app_id ?? null) !== (candidate.app_id ?? null)
        || result.artifact_kind !== candidate.artifact_kind || result.artifact_id !== candidate.artifact_id) return false
      if (result.status === 'deleted') return result.error_code === undefined
      if (result.status !== 'retained' || !['CLEANUP_ITEM_RETAINED', 'RELEASE_RETAINED', 'CLEANUP_ITEM_PROTECTED'].includes(String(result.error_code))) return false
      retained = true
      return true
    })
    return valid && (response.status === 'completed_with_failures') === retained
  }

  function candidateId(item: CleanupPreview['candidates'][number]): string {
    return item.artifact_id
  }

  function bytes(value: number): string {
    return new Intl.NumberFormat(undefined, { style: 'unit', unit: 'byte', notation: 'compact' }).format(value)
  }

  function protectionReason(reason: CleanupProtectionReason): string {
    switch (reason) {
      case 'active': return $t('Active release')
      case 'pending': return $t('Pending release')
      case 'current_draft': return $t('Current draft')
      case 'recent_rollback': return $t('Recent rollback')
      case 'deployment_recovery': return $t('Deployment recovery')
      case 'cleanup_in_progress': return $t('Cleanup in progress')
    }
  }
</script>

<section class="panel">
  <h2>{$t('Storage cleanup')}</h2>
  <p class="muted">{$t('Scan for old immutable releases, derived configuration revisions, and known temporary artifacts. SoloDock never cleans them automatically.')}</p>
  <p class="security-note">{$t('Active, pending, current draft, recovery references, and three recent rollback releases per application stay protected. Containers, volumes, binds, networks, credentials, deployments, and audit history are never removed here.')}</p>
  <p class="security-note">{$t('Cleaned deployment history remains visible, but rollback to a listed release becomes permanently unavailable.')}</p>
  {#if error}
    <p class="notice danger" role="alert">{requestId ? $t('{detail} (request {requestId})', { detail: messageText(error, $t), requestId }) : messageText(error, $t)}</p>
  {/if}
  {#if preview}
    <div class="configuration-stack">
      <p><strong>{$t('Cleanup candidates')}</strong> · {preview.candidates.length} · {$t('Estimated logical size')}: {bytes(preview.estimated_logical_bytes)}</p>
      {#if preview.candidates.length}
        <ul>
          {#each preview.candidates as item}
            <li><code>{item.artifact_kind}</code> · {candidateId(item)}{#if item.app_id} · {$t('Application')}: {item.app_id}{/if}{#if item.release_created_at} · {$t('Created')}: {item.release_created_at}{/if} · {bytes(item.estimated_logical_bytes)}</li>
          {/each}
        </ul>
      {:else}
        <p class="muted">{$t('No removable artifacts were found.')}</p>
      {/if}
      <p class="muted">{$t('{count} protected artifact references were found.', { count: preview.protected.reduce((sum, item) => sum + item.count, 0) })}</p>
      {#if preview.protected.length}
        <ul>
          {#each preview.protected as item}
            <li>{protectionReason(item.reason)} · {item.count}</li>
          {/each}
        </ul>
      {/if}
      <label><input type="checkbox" bind:checked={acknowledge} disabled={applying || Boolean(applyRetry)} /> {$t('I understand that cleanup permanently removes the listed rollback artifacts')}</label>
      <div class="actions">
        <button class="primary" disabled={!acknowledge || applying || preview.candidates.length === 0} onclick={() => void applyCleanup()}>{applying ? $t('Processing…') : applyRetry ? $t('Confirm exact cleanup result') : $t('Apply exact cleanup plan')}</button>
        <button class="ghost" disabled={applying || Boolean(applyRetry)} onclick={() => void scan()}>{$t('Scan again')}</button>
      </div>
    </div>
  {:else}
    <button disabled={scanning || applying} onclick={() => void scan()}>{scanning ? $t('Scanning…') : $t('Scan removable storage')}</button>
  {/if}
  {#if result}
    <div class="notice" role="status">
      <p>{result.status === 'completed' ? $t('Cleanup completed.') : $t('Cleanup completed with retained items. Scan again for current facts.')}</p>
      <ul>{#each result.items as item}<li>{item.artifact_kind} · {item.artifact_id} · {item.status}</li>{/each}</ul>
    </div>
  {/if}
</section>
