<script lang="ts">
  import { api, mutation } from '../lib/api'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import type { AppDetailResponse, AppMutationResponse } from '../lib/types'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { presetDescription } from '../lib/presets'
  let slug = $state('postgres')
  let major = $state('18')
  let username = $state('postgres')
  let database = $state('postgres')
  let password = $state(generatePassword())
  let busy = $state(false); let error = $state<UserMessage | null>(null); let copied = $state(false)
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
  function generatePassword() { const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'; const bytes = crypto.getRandomValues(new Uint8Array(24)); return Array.from(bytes, (value) => alphabet[value % alphabet.length]).join('') }
  async function copyPassword() { await navigator.clipboard.writeText(password); copied = true }
  async function create() {
    busy = true; error = null; copied = false
    try {
      if (!createdAppId) {
        const request = { slug, preset_id: 'postgresql', preset_schema_version: 1, variables: { major, username, database, password, initdb_args: '' } }
        createRetry = retryIdentity(createRetry, request)
        const created = await mutation<AppMutationResponse>('/api/v1/apps/from-preset', request, { idempotencyKey: createRetry.key })
        createdAppId = created.app.id; createRetry = undefined; password = ''
      }
      if (!deployRequest) {
        const app = await api<AppDetailResponse>(`/api/v1/apps/${createdAppId}`)
        deployRequest = { expected_draft_revision: app.draft_revision, expected_active_release_id: app.active_release?.id ?? null, expected_pending_release_id: app.pending_release_id, expected_actual_release_id: app.actual_release_id, expected_actual_container_id: app.actual?.id ?? null, acknowledge_non_rollbackable_data: true }
        deployRetry = retryIdentity(undefined, deployRequest)
      }
      if (!deployRetry) deployRetry = retryIdentity(undefined, deployRequest)
      const deployment = await mutation<{ deployment_id: string }>(`/api/v1/apps/${createdAppId}/deployments`, deployRequest, { idempotencyKey: deployRetry.key })
      deployRetry = undefined; deployRequest = null; window.location.hash = `/deployments/${deployment.deployment_id}`
    } catch { error = createdAppId ? localized('The service and configuration were created but not deployed. Retry here or continue from the service detail page.') : localized('Creation failed. If the network result is uncertain, the same password and idempotency key will be reused.') }
    finally { busy = false }
  }
</script>
<main class="page-shell narrow"><a class="back" href="#/apps/new">← {$t('Back to new service')}</a><div class="page-heading"><div><p class="eyebrow">{$t('QUICK DEPLOY')}</p><h1>PostgreSQL</h1><p class="muted">{presetDescription('postgresql', 'Single-instance PostgreSQL with a persistent volume and the platform service-discovery network.', $t)}</p><p class="muted">{$t('Only a service name is required by default. No host port is published; other services connect at {host}:5432.', { host: slug || 'postgres' })}</p></div></div>{#if error}<p class="notice danger" role="alert">{messageText(error, $t)}{#if createdAppId} <a href={`#/apps/${createdAppId}`}>{$t('Open service details')}</a>{/if}</p>{/if}<form class="panel configuration-stack" onsubmit={(event) => { event.preventDefault(); void create() }}><label>{$t('Service name')}<input bind:value={slug} maxlength="20" required disabled={createdAppId !== null} /></label><label>{$t('Major')}<select bind:value={major} disabled={createdAppId !== null}><option value="18">18 ({$t('Recommended')})</option><option value="17">17</option></select></label><label>{$t('Username')}<input bind:value={username} required disabled={createdAppId !== null} /></label><label>{$t('Database')}<input bind:value={database} required disabled={createdAppId !== null} /></label>{#if !createdAppId}<label>{$t('Generated password')}<input type="password" bind:value={password} required minlength="16" /><span class="muted">{$t('Copy and save it before creating the service. SoloDock never returns it after saving.')}</span></label>{/if}<div class="actions">{#if !createdAppId}<button type="button" class="ghost" onclick={() => { password = generatePassword(); copied = false }}>{$t('Regenerate')}</button><button type="button" class="ghost" onclick={() => void copyPassword()}>{copied ? $t('Copied') : $t('Copy password')}</button>{/if}<button disabled={busy}>{busy ? $t('Processing…') : createdAppId ? $t('Continue deployment') : $t('Create and deploy')}</button></div></form></main>
