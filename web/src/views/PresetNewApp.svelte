<script lang="ts">
  import { api, mutation } from '../lib/api'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import type { AppDetailResponse, AppMutationResponse } from '../lib/types'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { presetDescription } from '../lib/presets'

  let slug = $state('postgres')
  let major = $state('18')
  let username = $state('postgres')
  let database = $state('postgres')
  let password = $state(generatePassword())
  let acknowledgeNonRollbackableData = $state(false)
  let passwordSaved = $state(false)
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)
  let confirmationError = $state<UserMessage | null>(null)
  let copied = $state(false)
  let passwordGeneration = 0
  let copyGeneration = 0
  let createdAppId = $state<string | null>(null)
  let createRetry = $state<RetryIdentity | undefined>()
  let deployRetry = $state<RetryIdentity | undefined>()
  let deployRequest = $state<{
    expected_draft_revision: string | null
    expected_active_release_id: string | null
    expected_pending_release_id: string | null
    expected_actual_release_id: string | null
    expected_actual_container_id: string | null
    acknowledge_non_rollbackable_data: boolean
  } | null>(null)

  function generatePassword() {
    const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
    const bytes = crypto.getRandomValues(new Uint8Array(24))
    return Array.from(bytes, (value) => alphabet[value % alphabet.length]).join('')
  }

  function changePassword(value: string) {
    password = value
    passwordGeneration += 1
    passwordSaved = false
    copied = false
    confirmationError = null
  }

  function regeneratePassword() {
    changePassword(generatePassword())
  }

  async function copyPassword() {
    const value = password
    const passwordVersion = passwordGeneration
    const copyVersion = ++copyGeneration
    copied = false
    try {
      await navigator.clipboard.writeText(value)
      if (passwordVersion === passwordGeneration && copyVersion === copyGeneration) copied = true
    } catch {
      // A failed clipboard write must not imply that the password was copied.
    }
  }

  async function create() {
    if (!createdAppId && (!acknowledgeNonRollbackableData || !passwordSaved)) {
      confirmationError = localized('Confirm both PostgreSQL safety acknowledgements before creating the service.')
      return
    }

    const confirmedNonRollbackableData = acknowledgeNonRollbackableData
    let mutationStage: 'create' | 'deploy' | undefined
    busy = true
    error = null
    confirmationError = null
    copied = false
    try {
      if (!createdAppId) {
        const request = {
          slug,
          preset_id: 'postgresql',
          preset_schema_version: 1,
          variables: { major, username, database, password, initdb_args: '' },
        }
        createRetry = retryIdentity(createRetry, request)
        mutationStage = 'create'
        const created = await mutation<AppMutationResponse>('/api/v1/apps/from-preset', request, {
          idempotencyKey: createRetry.key,
        })
        createdAppId = created.app.id
        createRetry = undefined
        mutationStage = undefined
        password = ''
      }
      if (!deployRequest) {
        const app = await api<AppDetailResponse>(`/api/v1/apps/${createdAppId}`)
        deployRequest = {
          expected_draft_revision: app.draft_revision,
          expected_active_release_id: app.active_release?.id ?? null,
          expected_pending_release_id: app.pending_release_id,
          expected_actual_release_id: app.actual_release_id,
          expected_actual_container_id: app.actual?.id ?? null,
          acknowledge_non_rollbackable_data: confirmedNonRollbackableData,
        }
        deployRetry = retryIdentity(undefined, deployRequest)
      }
      if (!deployRetry) deployRetry = retryIdentity(undefined, deployRequest)
      mutationStage = 'deploy'
      const deployment = await mutation<{ deployment_id: string }>(
        `/api/v1/apps/${createdAppId}/deployments`,
        deployRequest,
        { idempotencyKey: deployRetry.key },
      )
      deployRetry = undefined
      deployRequest = null
      window.location.hash = `/deployments/${deployment.deployment_id}`
    } catch (cause) {
      const currentRetry = mutationStage === 'create'
        ? createRetry
        : mutationStage === 'deploy'
          ? deployRetry
          : undefined
      const failure = mutationFailure(currentRetry, cause)
      if (mutationStage === 'create') createRetry = failure.retry
      if (mutationStage === 'deploy') deployRetry = failure.retry
      error = createdAppId
        ? localized(mutationStage === 'deploy' && failure.outcome === 'outcome_unknown'
          ? 'The service and configuration were created, but the deployment outcome could not be confirmed. The generated password cannot be shown again. Retrying here will reuse the unchanged deployment request and idempotency key, or continue from the service detail page.'
          : mutationStage === 'deploy'
            ? 'The service and configuration were created, but deployment was rejected. The generated password cannot be shown again. Retrying here will use a new idempotency key, or continue from the service detail page.'
            : 'The service and configuration were created, but deployment could not be prepared. The generated password cannot be shown again. Retry here or continue from the service detail page.')
        : localized(failure.outcome === 'outcome_unknown'
          ? 'Creation failed. If the network result is uncertain, the same password and idempotency key will be reused.'
          : 'Creation was rejected. Review the fields before trying again; the next attempt will use a new idempotency key.')
    } finally {
      busy = false
    }
  }
</script>

<main class="page-shell narrow">
  <a class="back" href="#/apps/new">← {$t('Back to new service')}</a>
  <div class="page-heading">
    <div>
      <p class="eyebrow">{$t('QUICK DEPLOY')}</p>
      <h1>PostgreSQL</h1>
      <p class="muted">{presetDescription('postgresql', 'Single-instance PostgreSQL with a persistent volume and the platform service-discovery network.', $t)}</p>
      <p class="muted">{$t('Only a service name is required by default. No host port is published; other services connect at {host}:5432.', { host: slug || 'postgres' })}</p>
    </div>
  </div>
  {#if error}
    <p class="notice danger" role="alert">
      {messageText(error, $t)}
      {#if createdAppId} <a href={`#/apps/${createdAppId}`}>{$t('Open service details')}</a>{/if}
    </p>
  {/if}
  <form class="panel configuration-stack" onsubmit={(event) => { event.preventDefault(); void create() }}>
    <label>{$t('Service name')}<input bind:value={slug} maxlength="20" required disabled={busy || createdAppId !== null} /></label>
    <label>{$t('Major')}<select bind:value={major} disabled={busy || createdAppId !== null}><option value="18">18 ({$t('Recommended')})</option><option value="17">17</option></select></label>
    <label>{$t('Username')}<input bind:value={username} required disabled={busy || createdAppId !== null} /></label>
    <label>{$t('Database')}<input bind:value={database} required disabled={busy || createdAppId !== null} /></label>
    {#if !createdAppId}
      <label>
        {$t('Generated password')}
        <input
          type="password"
          value={password}
          oninput={(event) => changePassword(event.currentTarget.value)}
          required
          minlength="16"
          disabled={busy}
        />
        <span class="muted">{$t('Copy and save it before creating the service. SoloDock never returns it after saving.')}</span>
      </label>
      <label class="checkbox">
        <input
          type="checkbox"
          bind:checked={acknowledgeNonRollbackableData}
          onchange={() => { confirmationError = null }}
          required
          disabled={busy}
        />
        {$t('I understand that PostgreSQL data in the named volume does not roll back with a deployment or rollback')}
      </label>
      <label class="checkbox">
        <input
          type="checkbox"
          bind:checked={passwordSaved}
          onchange={() => { confirmationError = null }}
          required
          disabled={busy}
        />
        {$t('I saved the generated PostgreSQL password outside SoloDock')}
      </label>
      {#if confirmationError}<p class="form-error" role="alert">{messageText(confirmationError, $t)}</p>{/if}
    {/if}
    <div class="actions">
      {#if !createdAppId}
        <button type="button" class="ghost" disabled={busy} onclick={regeneratePassword}>{$t('Regenerate')}</button>
        <button type="button" class="ghost" disabled={busy} onclick={() => void copyPassword()}>{copied ? $t('Copied') : $t('Copy password')}</button>
      {/if}
      <button disabled={busy || (!createdAppId && (!acknowledgeNonRollbackableData || !passwordSaved))}>
        {busy ? $t('Processing…') : createdAppId ? $t('Continue deployment') : $t('Create and deploy')}
      </button>
    </div>
  </form>
</main>
