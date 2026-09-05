<script lang="ts">
  import { onDestroy } from 'svelte'
  import { ApiError, mutation } from '../lib/api'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'

  type Candidate = { image_id: string; manifest_digest: string; platform_os: string; platform_architecture: string; platform_variant: string | null; reported_size_bytes: number }
  type Preview = { candidates: Candidate[]; protected_count: number; confirmation_token: string; expires_at: string }
  type ApplyBody = { confirmation_token: string; image_ids: string[]; acknowledge_image_removal: true }
  type Result = { operation_id: string; plan_hash: string; status: 'completed' | 'completed_with_failures'; items: { image_id: string; status: 'removed' | 'retained' }[]; idempotency_replayed: boolean }
  let preview = $state<Preview | null>(null)
  let selected = $state<string[]>([])
  let acknowledge = $state(false)
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)
  let result = $state<Result | null>(null)
  let retry = $state<RetryIdentity | undefined>()
  let retained = $state<ApplyBody | undefined>()
  let generation = 0
  onDestroy(() => { generation++ })
  const imageId = /^sha256:[0-9a-f]{64}$/
  function object(value: unknown): value is Record<string, unknown> { return typeof value === 'object' && value !== null && !Array.isArray(value) }
  function validPreview(value: unknown): value is Preview {
    if (!object(value) || typeof value.confirmation_token !== 'string' || value.confirmation_token.length < 20
      || typeof value.expires_at !== 'string' || !Number.isFinite(Date.parse(value.expires_at))
      || !Number.isSafeInteger(value.protected_count) || Number(value.protected_count) < 0
      || !Array.isArray(value.candidates) || value.candidates.length > 100) return false
    const ids = new Set<string>()
    return value.candidates.every(c => {
      if (!object(c) || typeof c.image_id !== 'string' || !imageId.test(c.image_id) || ids.has(c.image_id)
        || typeof c.manifest_digest !== 'string' || !imageId.test(c.manifest_digest)
        || c.platform_os !== 'linux' || typeof c.platform_architecture !== 'string'
        || !/^[a-z0-9_]{1,20}$/.test(c.platform_architecture)
        || (c.platform_variant !== null && (typeof c.platform_variant !== 'string' || !/^[a-z0-9]{1,12}$/.test(c.platform_variant)))
        || !Number.isSafeInteger(c.reported_size_bytes) || Number(c.reported_size_bytes) < 0) return false
      ids.add(c.image_id)
      return true
    })
  }
  function validResult(value: unknown, expected: ApplyBody): value is Result {
    if (!object(value) || typeof value.operation_id !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value.operation_id)
      || typeof value.plan_hash !== 'string' || !/^[0-9a-f]{64}$/.test(value.plan_hash)
      || typeof value.idempotency_replayed !== 'boolean' || !Array.isArray(value.items) || value.items.length !== expected.image_ids.length) return false
    const items = value.items
    return items.every((item, index) => object(item) && item.image_id === expected.image_ids[index] && (item.status === 'removed' || item.status === 'retained'))
      && value.status === (items.some(item => item.status === 'retained') ? 'completed_with_failures' : 'completed')
  }
  function unconfirmed(): ApiError { return new ApiError(200, { code: 'HTTP_ERROR', message: 'Unconfirmed image cleanup response', request_id: '' }) }
  async function scan() {
    if (busy || retry) return
    const current = ++generation
    busy = true; error = null; result = null
    try {
      const value = await mutation<unknown>('/api/v1/system/image-cleanup/preview', {}, { expectedStatus: 200 })
      if (current !== generation) return
      if (!validPreview(value)) throw unconfirmed()
      preview = value; selected = []; acknowledge = false
    } catch {
      if (current === generation) { preview = null; error = localized('Could not create a safe image cleanup preview.') }
    } finally { if (current === generation) busy = false }
  }
  async function apply() {
    if (busy || !preview || !acknowledge || !selected.length) return
    const current = ++generation
    busy = true; error = null
    const body = retained ?? { confirmation_token: preview.confirmation_token, image_ids: [...selected].sort(), acknowledge_image_removal: true as const }
    retained = body; retry = retryIdentity(retry, body)
    try {
      const value = await mutation<unknown>('/api/v1/system/image-cleanup/apply', body, { idempotencyKey: retry.key, expectedStatus: 200 })
      if (current !== generation) return
      if (!validResult(value, body)) throw unconfirmed()
      result = value; preview = null; selected = []; acknowledge = false; retained = undefined; retry = undefined
    } catch (cause) {
      if (current !== generation) return
      const failure = mutationFailure(retry, cause)
      retry = failure.retry
      if (failure.outcome === 'outcome_unknown') error = localized('The image cleanup result is unknown. Retry only this exact operation; do not start another cleanup.')
      else {
        preview = null; selected = []; acknowledge = false; retained = undefined
        error = localized('The image cleanup was rejected. Scan again to review current references.')
      }
    } finally { if (current === generation) busy = false }
  }
</script>

<section class="panel">
  <h2>{$t('Docker image cleanup')}</h2>
  <p class="muted">{$t('Only images from confirmed cleaned releases are considered. Every retained release and every running or stopped container protects its images, including containers outside SoloDock.')}</p>
  <p class="security-note">{$t('Docker reported size is an upper estimate, not guaranteed reclaimed space or proof of ownership. Cleanup never removes containers, volumes, networks, or parent images and never forces deletion.')}</p>
  {#if error}<p class="notice danger" role="alert">{messageText(error, $t)}</p>{/if}
  {#if preview}
    <p>{$t('Protected images')}: {preview.protected_count}</p>
    {#if !preview.candidates.length}<p>{$t('No unused images are eligible for cleanup.')}</p>{/if}
    {#each preview.candidates as item}
      <label class="checkbox-row"><input type="checkbox" bind:group={selected} value={item.image_id} disabled={busy || !!retry} /><span><code>{item.image_id}</code> · {item.platform_os}/{item.platform_architecture}{item.platform_variant ? `/${item.platform_variant}` : ''} · {$t('Docker reported bytes')}: {item.reported_size_bytes}</span></label>
    {/each}
    <label class="checkbox-row"><input type="checkbox" bind:checked={acknowledge} disabled={busy || !!retry} />{$t('I confirm removal of only the selected unused images. This is separate from artifact cleanup.')}</label>
    <button class="button danger" disabled={busy || !acknowledge || !selected.length} onclick={apply}>{retry ? $t('Confirm the same image cleanup') : $t('Remove selected images')}</button>
  {/if}
  {#if result}
    <p role="status">{result.status === 'completed' ? $t('Selected image cleanup confirmed.') : $t('Some images were retained. Scan again before any new cleanup.')}</p>
    <ul>{#each result.items as item}<li><code>{item.image_id}</code> · {item.status === 'removed' ? $t('Removed') : $t('Retained')}</li>{/each}</ul>
  {/if}
  <button class="button secondary" disabled={busy || !!retry} onclick={scan}>{$t('Scan unused Docker images')}</button>
</section>
