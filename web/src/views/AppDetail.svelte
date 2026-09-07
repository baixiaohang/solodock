<script lang="ts">
  import { onMount } from 'svelte'
  import { ApiError, api, mutation } from '../lib/api'
  import { openSse } from '../lib/sse'
  import { configuredScopeText, driftText, formatBytes, mountKindText, networkKindText, networkModeText, shortRef, stateText } from '../lib/presentation'
  import { locale, localized, messageText, t, type MessageKey, type UserMessage } from '../lib/i18n'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { credentialsForReference } from '../lib/registryReference'
  import { canConfirmDeletion } from '../lib/deletionState'
  import { pollNeedsAttention, pollOutcomeText } from '../lib/pollingState'
  import { writeOnlyRetryIdentity } from '../lib/deploymentState'
  import { encodeWebhookSecret } from '../lib/webhookSecret'
  import { networkDraft, networkEditorError, networkEditorState } from '../lib/networks'
  import { formatTimestamp, timeSettings } from '../lib/time'
  import type { AppDetailResponse, ComposePlan, DeletionPreviewResponse, Deployment, DeploymentPage, DraftInput, ExternalNetworkAttachment, HealthConfigurationLimits, RegistryCredential, SettingsResponse, StatsSample, WebhookStatus } from '../lib/types'
  import LogsPane from '../components/LogsPane.svelte'
  import DeletionWebhookNotice from '../components/DeletionWebhookNotice.svelte'
  import NetworkEditor from '../components/NetworkEditor.svelte'
  import EnvironmentEditor from '../components/EnvironmentEditor.svelte'
  import PortEditor from '../components/PortEditor.svelte'
  import StorageEditor from '../components/StorageEditor.svelte'
  import HealthLifecycleEditor from '../components/HealthLifecycleEditor.svelte'
  import ManagedFileEditor from '../components/ManagedFileEditor.svelte'
  import ImageSuggestions from '../components/ImageSuggestions.svelte'
  import { buildEnvironmentProjection, clearSensitiveEnvironmentValues, environmentRowsFromDraft, type EnvironmentRow } from '../lib/environmentRows'
  import { buildManagedFileProjection, managedFileRowsFromDraft, type ManagedFileRow } from '../lib/managedFileRows'
  import { errorPresentation, errorPresentationText, FormValidationError, issuesUnder, remapIndexedIssues, type ErrorPresentation, type FormIssue } from '../lib/formErrors'
  let { appId }: { appId: string } = $props()
  let app = $state<AppDetailResponse | null>(null)
  let stats = $state<StatsSample | null>(null)
  let tab = $state<'overview' | 'configuration' | 'deployments' | 'logs'>('overview')
  let error = $state<UserMessage | null>(null)
  let formPresentation = $state<ErrorPresentation | null>(null)
  let visibleError = $derived(error ? messageText(error, $t) : formPresentation ? errorPresentationText(formPresentation, $t) : '')
  let actionBusy = $state(false)
  let deletion = $state<DeletionPreviewResponse | null>(null)
  let deletionDialog = $state(false)
  let confirmationSlug = $state('')
  let removeContainer = $state(false)
  let lifecycleKey = $state('')
  let lifecycleName = $state('')
  let deletionKey = $state('')
  let editing = $state(false)
  let editSession = $state(0)
  let editChanges = 0
  let editRevision = $state<string | null>(null)
  let editOriginallyAutoDeploy = false
  let draftConflict = $derived(editing && app !== null && app.draft_revision !== editRevision)
  let loading = $state(true)
  let loadError = $state<MessageKey | null>(null)
  let credentialsReady = false
  let settingsReady = false
  let webhookReady = false
  let deploymentError = $state(false)
  let credentialsError = $state(false)
  let settingsError = $state(false)
  let webhookError = $state(false)
  let loadGeneration = 0
  let loadController: AbortController | undefined
  let loadTimeout: ReturnType<typeof setTimeout> | undefined
  let refreshTimer: ReturnType<typeof setTimeout> | undefined
  let editRetry = $state<RetryIdentity | undefined>()
  let validation = $state<{ plan: ComposePlan; compose_yaml: string } | null>(null)
  let editName = $state('')
  let editImage = $state('')
  let editPoll = $state(300)
  let editStopGrace = $state(10)
  let editSecurityProfile = $state('')
  let editAutoDeploy = $state(false)
  let editEnvironmentRows = $state<EnvironmentRow[]>([])
  let editFileRows = $state<ManagedFileRow[]>([])
  let editPorts = $state<DraftInput['ports']>([])
  let editVolumes = $state<DraftInput['volumes']>([])
  let editBinds = $state<DraftInput['binds']>([])
  let editOwnedDefaultNetwork = $state(true)
  let editServiceDiscovery = $state(true)
  let editNetworks = $state<ExternalNetworkAttachment[]>([])
  let editHealth = $state<DraftInput['health']>({ policy: 'running', stable_window_seconds: 15 })
  let healthLimits = $state<HealthConfigurationLimits | null>(null)
  let formIssues = $state<FormIssue[]>([])
  let formIssueRequestId = $state<string | undefined>()
  let fileRequestRowIndexes = $state<number[]>([])
  let secretRequestRowIndexes = $state<number[]>([])
  let environmentEditor = $state<ReturnType<typeof EnvironmentEditor>>()
  let environmentClientIssue = $state<FormIssue | null>(null)
  let allowedBindRoots = $state<string[]>([])
  let credentials = $state<RegistryCredential[]>([])
  let editCredential = $state<string | null>(null)
  let deployments = $state<Deployment[]>([])
  let deployRetry = $state<RetryIdentity | undefined>()
  let webhook = $state<WebhookStatus | null>(null)
  let webhookSecret = $state('')
  let webhookSaved = $state(false)
  let webhookRetry = $state<RetryIdentity | undefined>()
  let disposed = false
  let matchingCredentials = $derived(credentialsForReference(credentials, editImage))
  let editNetworkError = $derived(networkEditorError({ ownedDefaultNetwork: editOwnedDefaultNetwork, serviceDiscoveryEnabled: editServiceDiscovery, externalNetworks: editNetworks }))

  onMount(() => {
    disposed = false
    void refresh(true)
    const onVisibility = () => {
      if (document.visibilityState === 'visible' && !actionBusy) void refresh(false)
    }
    document.addEventListener('visibilitychange', onVisibility)
    const source = openSse(`/api/v1/apps/${appId}/stats`, { stats: (event) => { if (!disposed) stats = JSON.parse(event.data) as StatsSample } })
    return () => {
      disposed = true
      loadGeneration++
      loadController?.abort()
      clearTimeout(loadTimeout)
      clearTimeout(refreshTimer)
      document.removeEventListener('visibilitychange', onVisibility)
      source.close()
      webhookSecret = ''; webhookSaved = false; webhookRetry = undefined
      lifecycleKey = ''; lifecycleName = ''; deletionKey = ''; confirmationSlug = ''
      editRetry = undefined; deployRetry = undefined; deletion = null
      clearSensitiveEnvironmentValues(editEnvironmentRows); editEnvironmentRows = []; editFileRows = []
    }
  })

  function scheduleRefresh() {
    clearTimeout(refreshTimer)
    if (!disposed) refreshTimer = setTimeout(() => {
      if (!actionBusy && document.visibilityState === 'visible') void refresh(false)
      else scheduleRefresh()
    }, 10_000)
  }

  async function refresh(auxiliary = true) {
    try { await load(auxiliary) } catch { /* load exposes a retryable page error. */ }
  }

  function invalidateLoad() {
    loadGeneration++
    loadController?.abort()
    clearTimeout(loadTimeout)
    clearTimeout(refreshTimer)
  }

  async function load(auxiliary = true) {
    if (disposed) return
    invalidateLoad()
    const generation = loadGeneration
    const controller = new AbortController()
    loadController = controller
    const current = () => !disposed && generation === loadGeneration
    const timeout = setTimeout(() => controller.abort(), 15_000)
    loadTimeout = timeout
    loading = !app
    const application = api<AppDetailResponse>(`/api/v1/apps/${appId}`, { signal: controller.signal }).then((value) => {
      if (current()) { app = value; loadError = null; loading = false }
    }).catch((cause) => {
      if (current()) {
        loading = false
        loadError = cause instanceof ApiError && (cause.status === 404 || cause.body.code === 'APP_NOT_FOUND')
          ? 'Application not found.' : app ? 'Could not refresh the application. Showing the last loaded state.' : 'Could not load the application.'
      }
      throw cause
    })
    const history = api<DeploymentPage>(`/api/v1/apps/${appId}/deployments?limit=20`, { signal: controller.signal }).then((page) => {
      if (current()) { deployments = page.items; deploymentError = false }
    }).catch(() => { if (current()) deploymentError = true })
    const requests: Promise<unknown>[] = [application, history]
    if (auxiliary || !credentialsReady) requests.push(
        api<RegistryCredential[]>('/api/v1/registry-credentials', { signal: controller.signal }).then((value) => {
          if (current()) { credentials = value; credentialsError = false; credentialsReady = true }
        }).catch(() => { if (current()) credentialsError = true }),
    )
    if (auxiliary || !webhookReady) requests.push(
        api<WebhookStatus>(`/api/v1/apps/${appId}/webhook`, { signal: controller.signal }).then((value) => {
          if (current()) { webhook = value; webhookError = false; webhookReady = true }
        }).catch(() => { if (current()) webhookError = true }),
    )
    if (auxiliary || !settingsReady) requests.push(
        api<SettingsResponse>('/api/v1/settings', { signal: controller.signal }).then((value) => {
          if (current()) {
            allowedBindRoots = value.allowed_bind_roots
            healthLimits = value.configuration_limits?.health ?? null
            settingsError = !healthLimits
            settingsReady = !!healthLimits
          }
        }).catch(() => { if (current()) settingsError = true }),
    )
    try {
      const results = await Promise.allSettled(requests)
      if (!current()) return false
      if (results[0].status === 'rejected') throw results[0].reason
      return true
    } finally {
      clearTimeout(timeout)
      if (current()) { loading = false; loadController = undefined; scheduleRefresh() }
    }
  }

  function factsConflict(cause: unknown): boolean {
    return cause instanceof ApiError && cause.mutationOutcome === 'known_not_applied'
      && cause.status === 409
  }

  function prepareWebhookSecret() {
    const bytes = crypto.getRandomValues(new Uint8Array(32))
    webhookSecret = encodeWebhookSecret(bytes)
    webhookSaved = false; webhookRetry = undefined
  }

  async function saveWebhook() {
    if (disposed || !webhook || !webhookSecret || !webhookSaved) return
    const request = { expected_metadata_revision: webhook.configured ? webhook.metadata_revision : null, secret: webhookSecret }
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null
    try {
      const nextRetry = await writeOnlyRetryIdentity(
        webhookRetry,
        { expected_metadata_revision: request.expected_metadata_revision },
        webhookSecret,
      )
      if (disposed) return
      webhookRetry = nextRetry
      const saved = await mutation<WebhookStatus>(`/api/v1/apps/${appId}/webhook`, request, { method: 'PUT', idempotencyKey: webhookRetry.key })
      if (disposed) return
      webhook = saved
      webhookSecret = ''; webhookSaved = false; webhookRetry = undefined
    } catch (cause) {
      if (!disposed) {
        const failure = mutationFailure(webhookRetry, cause)
        webhookRetry = failure.retry
        error = localized(failure.outcome === 'outcome_unknown'
          ? 'The secret mutation outcome could not be confirmed. Re-entering the same secret with unchanged fields will reuse its request identity.'
          : 'The secret mutation was not applied. Review the current state before re-entering it; the next attempt will use a new request identity.')
      }
    } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }

  async function revokeWebhook() {
    if (disposed || !webhook?.configured || !webhook.metadata_revision || !window.confirm($t('Revoking makes the old webhook secret invalid immediately. Periodic polling and already-claimed deployments are unaffected. Continue?'))) return
    const request = { expected_metadata_revision: webhook.metadata_revision }
    webhookRetry = retryIdentity(webhookRetry, request)
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null
    try {
      const revoked = await mutation<WebhookStatus>(`/api/v1/apps/${appId}/webhook`, request, { method: 'DELETE', idempotencyKey: webhookRetry.key })
      if (disposed) return
      webhook = revoked
      webhookSecret = ''; webhookSaved = false; webhookRetry = undefined
    } catch (cause) {
      if (!disposed) {
        const failure = mutationFailure(webhookRetry, cause)
        webhookRetry = failure.retry
        error = localized(failure.outcome === 'outcome_unknown'
          ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
          : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
      }
    } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }

  async function lifecycle(action: 'start' | 'stop' | 'restart') {
    if (disposed) return
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null
    if (lifecycleName !== action || !lifecycleKey) { lifecycleName = action; lifecycleKey = crypto.randomUUID() }
    try {
      await mutation(`/api/v1/apps/${appId}/actions/${action}`, undefined, { idempotencyKey: lifecycleKey })
      if (disposed) return
      lifecycleKey = ''; lifecycleName = ''
      try { await load() }
      catch { if (!disposed) error = localized('The lifecycle operation succeeded, but refreshing the application failed. Reload the page to see the latest state.') }
    }
    catch (cause) {
      if (!disposed) {
        const failure = mutationFailure(lifecycleKey || undefined, cause)
        lifecycleKey = failure.retry ?? ''
        if (!lifecycleKey) lifecycleName = ''
        error = localized(failure.outcome === 'outcome_unknown'
          ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
          : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
      }
    }
    finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }

  async function previewDeletion() {
    if (disposed) return
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null
    try {
      const preview = await mutation<DeletionPreviewResponse>(`/api/v1/apps/${appId}/deletion-preview`, { remove_container: removeContainer })
      if (disposed) return
      deletion = preview
      deletionKey = crypto.randomUUID()
      confirmationSlug = ''
    } catch { if (!disposed) error = localized('Could not generate the deletion preview.') } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }

  async function confirmDeletion() {
    if (disposed || !canConfirmDeletion(deletion, confirmationSlug, removeContainer)) return
    const confirmed = deletion
    if (!confirmed) return
    if (!deletionKey) deletionKey = crypto.randomUUID()
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null
    try {
      await mutation(`/api/v1/apps/${appId}`, {
        confirmation_token: confirmed.confirmation_token,
        slug: confirmationSlug,
        expected_revision: confirmed.expected_revision,
        remove_container: removeContainer,
      }, { method: 'DELETE', idempotencyKey: deletionKey })
      if (disposed) return
      deletion = null
      deletionKey = ''
      window.location.hash = '/'
    } catch (cause) {
      if (!disposed) {
        const failure = mutationFailure(deletionKey || undefined, cause)
        deletionKey = failure.retry ?? ''
        error = localized(failure.outcome === 'outcome_unknown'
          ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
          : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
      }
    } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }

  function pretty(value: unknown): string { return JSON.stringify(value, null, 2) }
  function retainedFact(name: string, scope: string, exists: boolean): string {
    return `${name} · ${configuredScopeText(scope, $t)} · ${exists ? $t('Exists') : $t('Configured only')}`
  }
  function retainedNetworkFact(item: DeletionPreviewResponse['retained']['networks'][number]): string {
    return `${retainedFact(item.name, item.configured_in, item.exists)} · ${networkKindText(item.kind, $t)}${item.bridge_name ? ` · ${$t('Bridge')}: ${item.bridge_name}` : ''}${item.aliases.length ? ` · ${$t('Aliases')}: ${item.aliases.join(', ')}` : ''}`
  }
  function startEditing() {
    if (!app) return
    editSession++
    editChanges = 0
    editRevision = app.draft_revision
    editOriginallyAutoDeploy = app.draft?.auto_deploy_enabled ?? false
    editName = app.display_name; editImage = app.draft?.discovery_image_ref ?? ''
    editPoll = app.draft?.poll_interval_seconds ?? 300
    editStopGrace = app.draft?.stop_grace_period_seconds ?? 10
    editSecurityProfile = app.draft?.security_profile ?? ''
    editAutoDeploy = app.draft?.auto_deploy_enabled ?? false
    editEnvironmentRows = app.draft ? environmentRowsFromDraft(app.draft) : []
    editFileRows = app.draft ? managedFileRowsFromDraft(app.draft) : []
    editPorts = (app.draft?.ports ?? []).map((row) => ({ ...row })); editVolumes = (app.draft?.volumes ?? []).map((row) => ({ ...row }))
    editBinds = (app.draft?.binds ?? []).map((row) => ({ ...row }))
    const networkState = networkEditorState(app.draft?.owned_default_network ?? true, app.draft?.service_discovery_enabled ?? true, app.draft?.networks ?? [])
    editOwnedDefaultNetwork = networkState.ownedDefaultNetwork
    editServiceDiscovery = app.draft?.service_discovery_enabled ?? true
    editNetworks = networkState.externalNetworks
    editHealth = JSON.parse(JSON.stringify(app.draft?.health ?? { policy: 'running', stable_window_seconds: healthLimits?.running_stable_window_seconds.default ?? 15 })); editRetry = undefined; validation = null; editing = true
    formIssues = []; formIssueRequestId = undefined; formPresentation = null; error = null; environmentClientIssue = null
    fileRequestRowIndexes = []; secretRequestRowIndexes = []
    editCredential = app.draft?.credential_ref ?? null
  }
  function buildDraft(): DraftInput {
    if (!app) throw new Error('missing app')
    if (!healthLimits || settingsError) throw new FormValidationError([{ path: 'health', code: 'CAPABILITIES_UNAVAILABLE', message: localized('Could not load backend health-check limits. Refresh and try again.') }])
    if (credentialsError) throw new FormValidationError([{ path: 'credential_ref', code: 'CAPABILITIES_UNAVAILABLE', message: localized('Could not load registry credentials. Retry before saving.') }])
    const environmentRows = environmentEditor?.prepare() ?? editEnvironmentRows
    const unacknowledgedBind = editBinds.findIndex((bind) => !bind.readonly && !bind.acknowledge_non_rollbackable)
    if (unacknowledgedBind >= 0) throw new FormValidationError([{
      path: `binds[${unacknowledgedBind}].acknowledge_non_rollbackable`,
      code: 'BIND_RW_ACK_REQUIRED',
      message: localized('Confirm that this read-write directory content does not roll back with a release'),
    }])
    const fileProjection = buildManagedFileProjection(editFileRows)
    fileRequestRowIndexes = fileProjection.requestRowIndexes
    const environmentProjection = buildEnvironmentProjection(environmentRows)
    secretRequestRowIndexes = environmentProjection.secretRequestRowIndexes
    return {
      display_name: editName, discovery_image_ref: editImage, credential_ref: editCredential,
      auto_deploy_enabled: editAutoDeploy, auto_deploy_acknowledged: editAutoDeploy && !editOriginallyAutoDeploy, poll_interval_seconds: editPoll,
      stop_grace_period_seconds: editStopGrace,
      security_profile: editSecurityProfile || null,
      environment: environmentProjection.environment,
      files: fileProjection.files,
      ports: editPorts,
      volumes: editVolumes,
      binds: editBinds,
      ...networkDraft({ ownedDefaultNetwork: editOwnedDefaultNetwork, serviceDiscoveryEnabled: editServiceDiscovery, externalNetworks: editNetworks }),
      service_discovery_enabled: editServiceDiscovery,
      health: editHealth,
    }
  }
  function setFormError(cause: unknown, fallback: MessageKey) {
    const presentation = errorPresentation(cause, localized(fallback))
    formIssues = cause instanceof FormValidationError
      ? presentation.issues
      : remapIndexedIssues(
          remapIndexedIssues(presentation.issues, 'files', fileRequestRowIndexes),
          'environment.secrets',
          secretRequestRowIndexes,
        )
    formIssueRequestId = presentation.requestId
    formPresentation = { ...presentation, issues: formIssues }
    error = null
  }
  function clearFormIssuePath(path: string) {
    editChanges++
    if (!formIssues.length) return
    formIssues = formIssues.filter((issue) => !(
      issue.path === path
      || issue.path.startsWith(`${path}.`)
      || issue.path.startsWith(`${path}[`)
      || path.startsWith(`${issue.path}.`)
      || path.startsWith(`${issue.path}[`)
    ))
    if (formPresentation) formPresentation = { ...formPresentation, issues: formIssues }
    if (!formIssues.length) { formIssueRequestId = undefined; formPresentation = null }
  }
  function handleFormInput(event: Event) {
    editChanges++
    const path = (event.target as HTMLElement).dataset.issuePath
    if (path) clearFormIssuePath(path)
  }
  async function deploy() {
    if (disposed || !app) return
    const nonRollbackable = (app.draft?.volumes.length ?? 0) > 0 || (app.draft?.binds.length ?? 0) > 0
    if (nonRollbackable && !window.confirm($t('Deployments and rollbacks do not revert named volume or bind contents. Continue?'))) return
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null
    const request = deployRetry ? JSON.parse(deployRetry.fingerprint) : {
      expected_draft_revision: app.draft_revision,
      expected_active_release_id: app.active_release?.id ?? null,
      expected_pending_release_id: app.pending_release_id,
      expected_actual_release_id: app.actual_release_id,
      expected_actual_container_id: app.actual?.id ?? null,
      acknowledge_non_rollbackable_data: true,
    }
    deployRetry = retryIdentity(deployRetry, request)
    try {
      const result = await mutation<{ deployment_id: string }>(`/api/v1/apps/${appId}/deployments`, request, { idempotencyKey: deployRetry.key })
      if (disposed) return
      deployRetry = undefined
      window.location.hash = `/deployments/${result.deployment_id}`
    } catch (cause) {
      if (!disposed) {
        const failure = mutationFailure(deployRetry, cause)
        deployRetry = failure.retry
        if (factsConflict(cause)) { await refresh(false); if (disposed) return }
        error = localized(failure.outcome === 'outcome_unknown'
          ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
          : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
      }
    } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }
  async function validateDraft() {
    if (disposed) return
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null; formIssues = []
    try {
      const result = await mutation<{ plan: ComposePlan; compose_yaml: string }>(`/api/v1/apps/${appId}/validate`, { draft: buildDraft() })
      if (disposed) return
      validation = result; error = null
    }
    catch (cause) {
      if (!disposed) setFormError(cause, 'Configuration validation failed. Check Docker/Compose status and try again.')
    } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }
  }
  async function saveDraft() {
    if (disposed || !app) return
    invalidateLoad()
    actionBusy = true; error = null; formPresentation = null; formIssues = []
    try {
      const request = { expected_revision: editRevision, draft: buildDraft() }
      const session = editSession
      const changes = editChanges
      const submittedSecrets = editEnvironmentRows.filter((row) => !row.removed && row.sensitive)
        .map((row) => ({ id: row.id, key: row.key, value: row.value }))
      const submittedFiles = editFileRows.filter((row) => !row.removed && row.sensitive)
        .map((row) => ({ row, name: row.logicalName, target: row.targetPath, value: row.value }))
      editRetry = retryIdentity(editRetry, request)
      await mutation(`/api/v1/apps/${appId}/draft`, request, { method: 'PUT', idempotencyKey: editRetry.key })
      if (disposed) return
      editRetry = undefined
      if (session === editSession) {
        for (const submitted of submittedSecrets) {
          const row = editEnvironmentRows.find((row) => row.id === submitted.id)
          if (row && !row.removed && row.sensitive && row.key === submitted.key && row.value === submitted.value) row.value = ''
        }
        for (const submitted of submittedFiles) {
          const row = submitted.row
          if (editFileRows.includes(row) && !row.removed && row.sensitive && row.logicalName === submitted.name
            && row.targetPath === submitted.target && row.value === submitted.value) row.value = ''
        }
      }
      try {
        const loaded = await load()
        if (loaded && !disposed && session === editSession && changes === editChanges) startEditing()
      } catch {
        if (!disposed) error = localized('The configuration was saved but refresh failed. Reopen the application page to load the latest revision.')
      }
    } catch (cause) {
      if (!disposed) {
        const failure = mutationFailure(editRetry, cause)
        editRetry = failure.retry
        if (factsConflict(cause)) { await refresh(false); if (disposed) return }
        setFormError(cause, failure.outcome === 'outcome_unknown'
          ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
          : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
      }
    } finally { if (!disposed) { actionBusy = false; scheduleRefresh() } }

  }
</script>

<main class="page-shell">
  <a class="back" href="#/">← {$t('Back to console')}</a>
  {#if visibleError}<p class="notice danger">{visibleError}</p>{/if}
  {#if loading}<p role="status">{$t('Loading application…')}</p>{/if}
  {#if loadError}<p class="notice danger" role="alert">{$t(loadError)} <button type="button" disabled={loading || actionBusy} onclick={() => void refresh(true)}>{$t('Retry loading')}</button></p>{/if}
  {#if deploymentError || credentialsError || settingsError || webhookError}
    <div class="notice warning" role="alert">
      {#if deploymentError}<p>{$t('Could not load deployment history. Previously loaded entries are preserved.')}</p>{/if}
      {#if credentialsError}<p>{$t('Could not load registry credentials. Retry before saving.')}</p>{/if}
      {#if settingsError}<p>{$t('Could not load backend health-check limits. Refresh and try again.')}</p>{/if}
      {#if webhookError}<p>{$t('Could not load webhook settings.')}</p>{/if}
      <button type="button" disabled={loading || actionBusy} onclick={() => void refresh(true)}>{$t('Retry loading')}</button>
    </div>
  {/if}
  {#if app}
    <div class="detail-heading"><div><p class="eyebrow">{$t('APPLICATION')}</p><h1>{app.display_name}</h1><code>{app.id}</code></div><span class:healthy={app.actual?.health === 'healthy'} class="state-pill large">{app.deployment_status === 'UNCONFIGURED' ? $t('Not configured') : app.deployment_status === 'DEPLOY_REQUIRED' ? $t('Waiting for first deployment') : `${stateText(app.actual?.status, $t)} · ${stateText(app.actual?.health ?? 'unknown', $t)}`}</span></div>
    <p class="notice"><strong>{$t('Immutable slug')}: </strong><code>{app.slug}</code> · <strong>{$t('Compose project')}: </strong><code>{app.resource_names.project_name}</code> · <strong>{$t('Default container')}: </strong><code>{app.resource_names.project_name}-app-1</code></p>
    {#if app.expected_owned_default_network}<p class="notice"><strong>{$t('Owned network')}: </strong><code>{app.expected_owned_default_network.docker_name}</code> · <strong>{$t('Host bridge')}: </strong><code>{app.expected_owned_default_network.bridge_name}</code></p>{:else if !app.active_release && !app.pending_release_id && app.draft?.owned_default_network}<p class="notice"><strong>{$t('Draft owned network')}: </strong><code>{app.resource_names.owned_default_network_name}</code> · <strong>{$t('Host bridge')}: </strong><code>{app.resource_names.bridge_name}</code></p>{/if}
    <div class="actions"><button disabled={actionBusy || (!app.available_actions.includes('deploy') && !deployRetry)} onclick={() => void deploy()}>{$t('Deploy draft')}</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('start')} onclick={() => void lifecycle('start')}>{$t('Start')}</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('stop')} onclick={() => void lifecycle('stop')}>{$t('Stop')}</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('restart')} onclick={() => void lifecycle('restart')}>{$t('Restart')}</button><button class="danger danger-action" disabled={actionBusy} onclick={() => { deletionDialog = true; deletion = null; removeContainer = false }}>{$t('Unregister…')}</button></div>
    <div class="tabs"><button class:active={tab === 'overview'} onclick={() => { tab = 'overview' }}>{$t('Overview')}</button><button class:active={tab === 'configuration'} onclick={() => { tab = 'configuration'; if (!editing) startEditing() }}>{$t('Configuration')}</button><button class:active={tab === 'deployments'} onclick={() => { tab = 'deployments' }}>{$t('Deployment history')}</button><button class:active={tab === 'logs'} onclick={() => { tab = 'logs' }}>{$t('Live logs')}</button></div>
    {#if tab === 'logs'}
      <LogsPane {appId} />
    {:else if tab === 'deployments'}
      <section class="panel deployment-history" aria-label={$t('Deployment history')}>
        {#if deployments.length}
          <div class="deployment-row deployment-header" aria-hidden="true"><span>{$t('Time')}</span><span>{$t('Status / phase')}</span><span>{$t('Trigger')}</span><span>{$t('Image / digest')}</span><span>{$t('Error')}</span><span></span></div>
          {#each deployments as deployment}
            <article class="deployment-row">
              <time datetime={deployment.created_at}>{formatTimestamp(deployment.created_at, $timeSettings.timezone, $locale)}</time>
              <span><strong>{stateText(deployment.status, $t)}</strong><small>{stateText(deployment.phase, $t)}</small></span>
              <span>{stateText(deployment.trigger, $t)}</span>
              <code title={deployment.source_image_ref ?? deployment.manifest_digest ?? deployment.candidate_release_id ?? $t('Resolving')}>{deployment.source_image_ref ?? deployment.manifest_digest ?? deployment.candidate_release_id ?? $t('Resolving')}</code>
              <span>{deployment.error_code ?? '—'}</span>
              <a href={`#/deployments/${deployment.id}`}>{$t('View details')}</a>
            </article>
          {/each}
        {:else if !deploymentError}<p class="muted">{$t('No deployment history.')}</p>{/if}
      </section>
    {:else if tab === 'configuration'}
      {#if editing}
        <form class="panel configuration-stack" oninput={handleFormInput} onchange={handleFormInput} onsubmit={(event) => { event.preventDefault(); void saveDraft() }}>
          <header><h2>{$t('Draft configuration')}</h2><p class="muted">{$t('Revision {revision}. Saving atomically creates a new immutable revision.', { revision: editRevision ?? $t('Not created') })}</p></header>
          {#if draftConflict}<p class="notice warning" role="alert">{$t('The draft changed elsewhere. Your inputs and original revision are preserved. Reload the draft to discard these edits.')} <button type="button" class="ghost" disabled={actionBusy} onclick={startEditing}>{$t('Reload draft')}</button></p>{/if}
          <label>{$t('Slug (immutable)')}<input value={app.slug} readonly /></label><label>{$t('Display name')}<input data-issue-path="display_name" bind:value={editName} required /></label>
          <label>{$t('Discovery image tag')}<input data-issue-path="discovery_image_ref" bind:value={editImage} aria-invalid={formIssues.some((issue) => issue.path === 'discovery_image_ref') ? 'true' : undefined} required /></label><label>{$t('Poll interval (seconds)')}<input data-issue-path="poll_interval_seconds" type="number" min="60" max="86400" bind:value={editPoll} aria-invalid={formIssues.some((issue) => issue.path === 'poll_interval_seconds') ? 'true' : undefined} /></label>
          <label class="checkbox"><input data-issue-path="auto_deploy_enabled" type="checkbox" bind:checked={editAutoDeploy} /> {$t('Automatically deploy new digests for the tag')}</label>
          {#if editAutoDeploy}<p class="notice warning">{$t('When enabled, a new digest automatically replaces the container and restores the old release if health checks fail. Volume and bind data do not roll back. Disabling does not cancel deployments that are already durably claimed.')}</p>{/if}
          <label>{$t('Registry credential')}<select data-issue-path="credential_ref" bind:value={editCredential} disabled={credentialsError}>{#if editCredential && !matchingCredentials.some((credential) => credential.id === editCredential)}<option disabled value={editCredential}>{editCredential}</option>{/if}<option value={null}>{$t('Anonymous')}</option>{#each matchingCredentials as credential}<option value={credential.id}>{credential.registry} · {credential.username}</option>{/each}</select></label>
          <ImageSuggestions image={editImage} credentialRef={editCredential} bind:ports={editPorts} bind:volumes={editVolumes} onStructureChange={clearFormIssuePath} />
          {#key editSession}<EnvironmentEditor bind:this={environmentEditor} bind:rows={editEnvironmentRows} bind:clientIssue={environmentClientIssue} issues={issuesUnder(formIssues, 'environment')} onStructureChange={clearFormIssuePath} />{/key}
          <ManagedFileEditor bind:rows={editFileRows} issues={issuesUnder(formIssues, 'files')} onStructureChange={clearFormIssuePath} />
          <PortEditor bind:ports={editPorts} issues={issuesUnder(formIssues, 'ports')} onStructureChange={clearFormIssuePath} />
          <StorageEditor bind:volumes={editVolumes} bind:binds={editBinds} {allowedBindRoots} issues={[...issuesUnder(formIssues, 'volumes'), ...issuesUnder(formIssues, 'binds')]} onStructureChange={clearFormIssuePath} />
          <NetworkEditor bind:ownedDefaultNetwork={editOwnedDefaultNetwork} bind:serviceDiscoveryEnabled={editServiceDiscovery} bind:externalNetworks={editNetworks} issues={issuesUnder(formIssues, 'networks')} onStructureChange={clearFormIssuePath} />
          <article class="panel"><label>{$t('Container security profile')}<input data-issue-path="security_profile" bind:value={editSecurityProfile} placeholder="codex-v1" /></label><p>{$t('Leave empty for Docker defaults. Use a versioned profile installed by the host operator.')}</p></article>
          <HealthLifecycleEditor bind:health={editHealth} bind:stopGrace={editStopGrace} limits={healthLimits} issues={[...issuesUnder(formIssues, 'health'), ...issuesUnder(formIssues, 'stop_grace_period_seconds')]} />
          <div class="actions"><button type="button" class="ghost" disabled={actionBusy || !!editNetworkError || !healthLimits || settingsError || credentialsError} onclick={() => void validateDraft()}>{$t('Validate only')}</button><button disabled={actionBusy || !!editNetworkError || !healthLimits || settingsError || credentialsError}>{$t('Save new revision')}</button><button type="button" class="ghost" onclick={() => { clearSensitiveEnvironmentValues(editEnvironmentRows); editFileRows = []; editRetry = undefined; editing = false; editSession++; tab = 'overview' }}>{$t('Cancel')}</button></div>
          {#if validation}<article class="notice"><h3>{$t('Compose validation')}</h3><p>{validation.plan.runnable ? $t('Runnable') : $t('Preview only')} · {$t('{grace} second stop grace · {ports} ports · {mounts} mounts · {networks} networks · {mode}', { grace: validation.plan.stop_grace_period_seconds, ports: validation.plan.ports, mounts: validation.plan.mounts, networks: validation.plan.networks, mode: networkModeText(validation.plan.network_mode, $t) })}</p>{#if validation.plan.owned_default_network}<p>{$t('Owned network')}: <code>{validation.plan.owned_default_network.docker_name}</code> · {$t('Bridge')}: <code>{validation.plan.owned_default_network.bridge_name}</code></p>{/if}{#each validation.plan.external_networks as network}<p><code>{network.name}</code>{#if network.aliases.length} · {$t('Aliases')}: {network.aliases.join(', ')}{/if}</p>{/each}{#if validation.plan.external_networks.length}<p>{$t('External networks are not created, changed, or deleted by SoloDock.')}</p>{/if}{#each validation.plan.warnings as warning}<span class="tag">{warning}</span>{/each}<pre>{validation.compose_yaml}</pre></article>{/if}
        </form>
        {#if app.draft && webhook}<article class="panel webhook-panel"><h2>{$t('Registry recheck webhook')}</h2><p><span class="tag">{webhook.degraded ? $t('Configuration damaged; generate a new secret to repair it') : webhook.configured ? $t('Configured') : $t('Not configured')}</span> · {webhook.algorithm}</p><p><code>{webhook.public_origin}{webhook.public_path}</code></p><p class="muted">{$t('The webhook triggers one durable Registry recheck. It does not trust image information in the payload or bypass automatic deployment, backoff, drift checks, or health gates.')}</p>{#if webhookSecret}<p class="notice warning">{$t('This is the only time the secret is displayed. Save it in your CI secret store:')} <code>{webhookSecret}</code></p><label class="checkbox"><input type="checkbox" bind:checked={webhookSaved} /> {$t('I saved this secret securely')}</label><div class="actions"><button disabled={actionBusy || !webhookSaved} onclick={() => void saveWebhook()}>{webhook.configured ? $t('Confirm rotation') : $t('Confirm configuration')}</button><button class="ghost" onclick={() => { webhookSecret = ''; webhookSaved = false; webhookRetry = undefined }}>{$t('Cancel')}</button></div>{:else}<div class="actions"><button disabled={actionBusy} onclick={prepareWebhookSecret}>{webhook.configured ? $t('Generate rotation secret') : $t('Generate webhook secret')}</button>{#if webhook.configured}<button class="danger" disabled={actionBusy} onclick={() => void revokeWebhook()}>{$t('Revoke webhook')}</button>{/if}</div>{/if}</article>{/if}
      {/if}
    {:else}
      {#if app.drift_codes.length}<div class="notice warning">{#each app.drift_codes as code}<span>{driftText(code, $t)}</span>{/each}</div>{/if}
      <section class="detail-grid">
        <article class="panel"><h2>{$t('Release comparison')}</h2><dl class="fact-list"><div><dt>{$t('Active image')}</dt><dd><code>{shortRef(app.active_release?.image_ref)}</code></dd></div><div><dt>{$t('Actual image')}</dt><dd><code>{shortRef(app.actual?.configured_image_ref)}</code></dd></div><div><dt>{$t('Container ID')}</dt><dd><code>{app.actual?.id.slice(0, 12) ?? '—'}</code></dd></div><div><dt>{$t('Restart count')}</dt><dd>{app.actual?.restart_count ?? '—'}</dd></div><div><dt>{$t('Exit code')}</dt><dd>{app.actual?.exit_code ?? '—'}</dd></div></dl></article>
        <article class="panel"><h2>{$t('Live resources')}</h2><dl class="fact-list"><div><dt>{$t('CPU')}</dt><dd>{stats?.cpu_percent?.toFixed(2) ?? '—'}%</dd></div><div><dt>{$t('Memory')}</dt><dd>{formatBytes(stats?.memory_usage_bytes ?? null)} / {formatBytes(stats?.memory_limit_bytes ?? null)}</dd></div><div><dt>{$t('Received')}</dt><dd>{formatBytes(stats?.network_rx_bytes ?? null)}</dd></div><div><dt>{$t('Sent')}</dt><dd>{formatBytes(stats?.network_tx_bytes ?? null)}</dd></div></dl></article>
        <article class="panel wide"><h2>{$t('Automatic deployment')}</h2><dl class="fact-list"><div><dt>{$t('Status')}</dt><dd>{app.draft?.auto_deploy_enabled ? $t('Enabled') : $t('Disabled')}</dd></div><div><dt>{$t('Last result')}</dt><dd class:warning={pollNeedsAttention(app.polling)}>{pollOutcomeText(app.polling, $t)}</dd></div><div><dt>{$t('Last checked')}</dt><dd><time datetime={app.polling?.last_checked_at ?? undefined}>{formatTimestamp(app.polling?.last_checked_at, $timeSettings.timezone, $locale)}</time></dd></div><div><dt>{$t('Next check not before')}</dt><dd><time datetime={app.polling?.next_check_not_before ?? undefined}>{formatTimestamp(app.polling?.next_check_not_before, $timeSettings.timezone, $locale)}</time></dd></div><div><dt>{$t('Manifest')}</dt><dd><code>{app.polling?.last_manifest_digest ?? '—'}</code></dd></div><div><dt>{$t('Platform')}</dt><dd>{app.polling?.last_platform ?? '—'}</dd></div><div><dt>{$t('Error')}</dt><dd>{app.polling?.last_error_code ?? $t('None')}</dd></div></dl>{#if app.polling?.suppressed_deployment_id}<a href={`#/deployments/${app.polling.suppressed_deployment_id}`}>{$t('View suppressed failed deployment')}</a>{/if}</article>
        <article class="panel wide"><h2>{$t('Ports')}</h2>{#each app.actual?.ports ?? [] as port}<p><code>{port.host_ip}:{port.host_port}</code> → {port.container_port}/{port.protocol}</p>{:else}<p class="muted">{$t('No loopback port mappings')}</p>{/each}</article>
        <article class="panel"><h2>{$t('Mounts')}</h2>{#each app.actual?.mounts ?? [] as mount}<p><span class="tag">{mountKindText(mount.kind, $t)}</span> {mount.destination} · {mount.read_only ? $t('Read-only') : $t('Read-write')}</p>{:else}<p class="muted">{$t('No mounts')}</p>{/each}</article>
        <article class="panel wide"><h2>{$t('Networks')}</h2><div class="network-comparison"><section><h3>{$t('Expected')}</h3>{#if app.expected_network_plan}<p><span class="tag">{networkModeText(app.expected_network_plan.mode, $t)}</span></p>{#if app.expected_owned_default_network}<p><code>{app.expected_owned_default_network.docker_name}</code> · {$t('Bridge')}: <code>{app.expected_owned_default_network.bridge_name}</code></p>{/if}{#each app.expected_network_plan.external as network}<p><code>{network.name}</code>{#if network.aliases.length} · {$t('Aliases')}: {network.aliases.join(', ')}{/if}</p>{/each}{:else}<p class="muted">{$t('No immutable release network expectation is available')}</p>{/if}</section><section><h3>{$t('Actual')}</h3>{#if app.actual_owned_default_network}<p><code>{app.actual_owned_default_network.docker_name}</code> · {app.actual_owned_default_network.driver ?? $t('Unknown')} · {$t('Bridge')}: <code>{app.actual_owned_default_network.bridge_name ?? $t('Not set')}</code></p>{/if}{#each app.actual?.networks ?? [] as network}<p><code>{network.name}</code> · {network.container_ip ?? '—'}{network.aliases.length ? ` · DNS: ${network.aliases.join(', ')}` : ''}</p>{:else}<p class="muted">{$t('No container network attachments')}</p>{/each}</section></div></article>
      </section>
    {/if}
  {/if}
  {#if deletionDialog}
    <div class="modal-backdrop" role="presentation"><div class="modal" role="dialog" aria-modal="true" aria-label={$t('Confirm unregistration')}><h2>{$t('Confirm unregistration')}</h2><p class="notice warning">{$t('By default, only the catalog entry is removed. Containers, named volumes, bind contents, and networks are retained.')}{deletion?.orphan_warning ? $t('The existing container will become an orphan.') : ''}</p><DeletionWebhookNotice configured={deletion?.webhook_configured ?? false} /><label class="checkbox"><input type="checkbox" bind:checked={removeContainer} disabled={deletion !== null} /> {$t('Also remove the exact owned container (data resources remain)')}</label>{#if deletion}<section class="deletion-preview"><p><strong>{$t('Compose project')}: </strong><code>{deletion.project_name}</code></p><p><strong>{$t('Active release')}: </strong><code>{deletion.active_release_id ?? $t('None')}</code></p><p><strong>{$t('Active config')}: </strong><code>{deletion.active_config_revision ?? $t('None')}</code></p><p><strong>{$t('Pending release')}: </strong><code>{deletion.pending_release_id ?? $t('None')}</code></p><p><strong>{$t('Pending config')}: </strong><code>{deletion.pending_config_revision ?? $t('None')}</code></p><p><strong>{$t('Preview expires')}: </strong><time datetime={deletion.expires_at}>{formatTimestamp(deletion.expires_at, $timeSettings.timezone, $locale)}</time></p><p><strong>{$t('Containers')}: </strong>{deletion.container_ids.length ? deletion.container_ids.join(', ') : $t('None')}</p><p><strong>{$t('Managed files')}: </strong>{deletion.managed_files.length ? deletion.managed_files.map((file) => `${file.logical_name} · ${configuredScopeText(file.configured_in, $t)}`).join(', ') : $t('None')}</p><p><strong>{$t('Retained owned volumes')}: </strong>{deletion.retained.owned_volumes.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || $t('None')}</p><p><strong>{$t('Retained external volumes')}: </strong>{deletion.retained.external_volumes.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || $t('None')}</p><p><strong>{$t('Retained binds')}: </strong>{deletion.retained.binds.map((bind) => `${retainedFact(bind.source, bind.configured_in, bind.exists)} (${bind.readonly ? 'ro' : 'rw'})`).join(', ') || $t('None')}</p><p><strong>{$t('Retained networks')}: </strong>{deletion.retained.networks.map(retainedNetworkFact).join(', ') || $t('None')}</p></section><label>{$t('Enter {slug} to confirm', { slug: deletion.slug })}<input bind:value={confirmationSlug} autocomplete="off" /></label><div class="actions"><button class="danger" disabled={actionBusy || confirmationSlug !== deletion.slug || Date.parse(deletion.expires_at) <= Date.now()} onclick={() => void confirmDeletion()}>{$t('Confirm unregistration')}</button><button class="ghost" onclick={() => { deletion = null; deletionDialog = false; confirmationSlug = '' }}>{$t('Cancel')}</button></div>{:else}<div class="actions"><button class="danger" disabled={actionBusy} onclick={() => void previewDeletion()}>{$t('Generate exact deletion preview')}</button><button class="ghost" onclick={() => { deletionDialog = false }}>{$t('Cancel')}</button></div>{/if}</div></div>
  {/if}
</main>
