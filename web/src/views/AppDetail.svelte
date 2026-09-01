<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import { openSse } from '../lib/sse'
  import { driftText, formatBytes, shortRef } from '../lib/presentation'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { credentialsForReference } from '../lib/registryReference'
  import { canConfirmDeletion } from '../lib/deletionState'
  import { pollNeedsAttention, pollOutcomeText } from '../lib/pollingState'
  import { writeOnlyRetryIdentity } from '../lib/deploymentState'
  import { encodeWebhookSecret } from '../lib/webhookSecret'
  import { networkDraft, networkEditorError, networkEditorState } from '../lib/networks'
  import { formatTimestamp, timeSettings } from '../lib/time'
  import type { AppDetailResponse, ComposePlan, DeletionPreviewResponse, Deployment, DeploymentPage, DraftInput, ExternalNetworkAttachment, RegistryCredential, SettingsResponse, StatsSample, WebhookStatus } from '../lib/types'
  import LogsPane from '../components/LogsPane.svelte'
  import DeletionWebhookNotice from '../components/DeletionWebhookNotice.svelte'
  import NetworkEditor from '../components/NetworkEditor.svelte'
  import EnvironmentEditor from '../components/EnvironmentEditor.svelte'
  import PortEditor from '../components/PortEditor.svelte'
  import StorageEditor from '../components/StorageEditor.svelte'
  import HealthLifecycleEditor from '../components/HealthLifecycleEditor.svelte'
  import ManagedFileEditor from '../components/ManagedFileEditor.svelte'
  import ImageSuggestions from '../components/ImageSuggestions.svelte'
  import { buildEnvironment, clearSensitiveEnvironmentValues, environmentRowsFromDraft, type EnvironmentRow } from '../lib/environmentRows'
  import { buildManagedFiles, managedFileRowsFromDraft, type ManagedFileRow } from '../lib/managedFileRows'
  let { appId }: { appId: string } = $props()
  let app = $state<AppDetailResponse | null>(null)
  let stats = $state<StatsSample | null>(null)
  let tab = $state<'overview' | 'configuration' | 'deployments' | 'logs'>('overview')
  let error = $state('')
  let actionBusy = $state(false)
  let deletion = $state<DeletionPreviewResponse | null>(null)
  let deletionDialog = $state(false)
  let confirmationSlug = $state('')
  let removeContainer = $state(false)
  let lifecycleKey = $state('')
  let lifecycleName = $state('')
  let deletionKey = $state('')
  let editing = $state(false)
  let editRetry = $state<RetryIdentity | undefined>()
  let validation = $state<{ plan: ComposePlan; compose_yaml: string } | null>(null)
  let editName = $state('')
  let editImage = $state('')
  let editPoll = $state(300)
  let editStopGrace = $state(10)
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
  let allowedBindRoots = $state<string[]>([])
  let credentials = $state<RegistryCredential[]>([])
  let editCredential = $state<string | null>(null)
  let deployments = $state<Deployment[]>([])
  let deployRetry = $state<RetryIdentity | undefined>()
  let webhook = $state<WebhookStatus | null>(null)
  let webhookSecret = $state('')
  let webhookSaved = $state(false)
  let webhookRetry = $state<RetryIdentity | undefined>()
  let matchingCredentials = $derived(credentialsForReference(credentials, editImage))
  let editNetworkError = $derived(networkEditorError({ ownedDefaultNetwork: editOwnedDefaultNetwork, serviceDiscoveryEnabled: editServiceDiscovery, externalNetworks: editNetworks }))
  $effect(() => {
    if (editCredential && credentials.length > 0 && !matchingCredentials.some((value) => value.id === editCredential)) editCredential = null
  })

  onMount(() => {
    void load()
    const source = openSse(`/api/v1/apps/${appId}/stats`, { stats: (event) => { stats = JSON.parse(event.data) as StatsSample } })
    return () => { source.close(); webhookSecret = ''; webhookRetry = undefined }
  })

  async function load() {
    const [loadedApp, page, loadedCredentials, loadedWebhook, loadedSettings] = await Promise.all([
      api<AppDetailResponse>(`/api/v1/apps/${appId}`),
      api<DeploymentPage>(`/api/v1/apps/${appId}/deployments?limit=20`),
      api<RegistryCredential[]>('/api/v1/registry-credentials'),
      api<WebhookStatus>(`/api/v1/apps/${appId}/webhook`).catch(() => null),
      api<SettingsResponse>('/api/v1/settings').catch(() => ({ revision: '', display_timezone: 'UTC', supported_timezones: ['UTC'], allowed_bind_roots: [], slug_max_length: 20, supported_mount_types: ['owned_volume', 'external_volume', 'bind'] })),
    ])
    app = loadedApp; deployments = page.items; credentials = loadedCredentials; webhook = loadedWebhook; allowedBindRoots = loadedSettings.allowed_bind_roots
  }

  function prepareWebhookSecret() {
    const bytes = crypto.getRandomValues(new Uint8Array(32))
    webhookSecret = encodeWebhookSecret(bytes)
    webhookSaved = false; webhookRetry = undefined
  }

  async function saveWebhook() {
    if (!webhook || !webhookSecret || !webhookSaved) return
    const request = { expected_metadata_revision: webhook.configured ? webhook.metadata_revision : null, secret: webhookSecret }
    webhookRetry = await writeOnlyRetryIdentity(
      webhookRetry,
      { expected_metadata_revision: request.expected_metadata_revision },
      webhookSecret,
    )
    actionBusy = true; error = ''
    try {
      webhook = await mutation<WebhookStatus>(`/api/v1/apps/${appId}/webhook`, request, { method: 'PUT', idempotencyKey: webhookRetry.key })
      webhookSecret = ''; webhookSaved = false; webhookRetry = undefined
    } catch { error = 'Webhook 配置保存失败；网络结果不明确时会复用同一 secret 和幂等键。' } finally { actionBusy = false }
  }

  async function revokeWebhook() {
    if (!webhook?.configured || !webhook.metadata_revision || !window.confirm('撤销后旧 webhook secret 立即失效；周期轮询和已 claim 部署不受影响。继续？')) return
    const request = { expected_metadata_revision: webhook.metadata_revision }
    webhookRetry = retryIdentity(webhookRetry, request)
    actionBusy = true; error = ''
    try {
      webhook = await mutation<WebhookStatus>(`/api/v1/apps/${appId}/webhook`, request, { method: 'DELETE', idempotencyKey: webhookRetry.key })
      webhookSecret = ''; webhookSaved = false; webhookRetry = undefined
    } catch { error = 'Webhook 撤销失败；请刷新状态后重试。' } finally { actionBusy = false }
  }

  async function lifecycle(action: 'start' | 'stop' | 'restart') {
    actionBusy = true; error = ''
    if (lifecycleName !== action || !lifecycleKey) { lifecycleName = action; lifecycleKey = crypto.randomUUID() }
    try { await mutation(`/api/v1/apps/${appId}/actions/${action}`, undefined, { idempotencyKey: lifecycleKey }); lifecycleKey = ''; lifecycleName = ''; await load() }
    catch { error = '生命周期操作失败；应用状态或 Docker/Compose 能力可能已变化。' }
    finally { actionBusy = false }
  }

  async function previewDeletion() {
    actionBusy = true; error = ''
    try {
      deletion = await mutation(`/api/v1/apps/${appId}/deletion-preview`, { remove_container: removeContainer })
      deletionKey = crypto.randomUUID()
      confirmationSlug = ''
    } catch { error = '无法生成删除预览。' } finally { actionBusy = false }
  }

  async function confirmDeletion() {
    if (!canConfirmDeletion(deletion, confirmationSlug, removeContainer)) return
    const confirmed = deletion
    if (!confirmed) return
    actionBusy = true; error = ''
    try {
      await mutation(`/api/v1/apps/${appId}`, {
        confirmation_token: confirmed.confirmation_token,
        slug: confirmationSlug,
        expected_revision: confirmed.expected_revision,
        remove_container: removeContainer,
      }, { method: 'DELETE', idempotencyKey: deletionKey })
      deletion = null
      deletionKey = ''
      window.location.hash = '/'
    } catch { error = '删除预览已失效或应用状态发生变化，请重新生成。' } finally { actionBusy = false }
  }

  function pretty(value: unknown): string { return JSON.stringify(value, null, 2) }
  function retainedFact(name: string, scope: string, exists: boolean): string {
    return `${name} · ${scope} · ${exists ? '实际存在' : '仅配置'}`
  }
  function startEditing() {
    if (!app) return
    editName = app.display_name; editImage = app.draft?.discovery_image_ref ?? ''
    editPoll = app.draft?.poll_interval_seconds ?? 300
    editStopGrace = app.draft?.stop_grace_period_seconds ?? 10
    editAutoDeploy = app.draft?.auto_deploy_enabled ?? false
    editEnvironmentRows = app.draft ? environmentRowsFromDraft(app.draft) : []
    editFileRows = app.draft ? managedFileRowsFromDraft(app.draft) : []
    editPorts = (app.draft?.ports ?? []).map((row) => ({ ...row })); editVolumes = (app.draft?.volumes ?? []).map((row) => ({ ...row }))
    editBinds = (app.draft?.binds ?? []).map((row) => ({ ...row }))
    const networkState = networkEditorState(app.draft?.owned_default_network ?? true, app.draft?.service_discovery_enabled ?? true, app.draft?.networks ?? [])
    editOwnedDefaultNetwork = networkState.ownedDefaultNetwork
    editServiceDiscovery = app.draft?.service_discovery_enabled ?? true
    editNetworks = networkState.externalNetworks
    editHealth = JSON.parse(JSON.stringify(app.draft?.health ?? { policy: 'running', stable_window_seconds: 15 })); editRetry = undefined; validation = null; editing = true
    editCredential = app.draft?.credential_ref ?? null
  }
  function buildDraft(): DraftInput {
    if (!app) throw new Error('missing app')
    return {
      display_name: editName, discovery_image_ref: editImage, credential_ref: editCredential,
      auto_deploy_enabled: editAutoDeploy, auto_deploy_acknowledged: editAutoDeploy && !(app.draft?.auto_deploy_enabled ?? false), poll_interval_seconds: editPoll,
      stop_grace_period_seconds: editStopGrace,
      environment: buildEnvironment(editEnvironmentRows),
      files: buildManagedFiles(editFileRows),
      ports: editPorts,
      volumes: editVolumes,
      binds: editBinds,
      ...networkDraft({ ownedDefaultNetwork: editOwnedDefaultNetwork, serviceDiscoveryEnabled: editServiceDiscovery, externalNetworks: editNetworks }),
      service_discovery_enabled: editServiceDiscovery,
      health: editHealth,
    }
  }
  async function deploy() {
    if (!app) return
    const nonRollbackable = (app.draft?.volumes.length ?? 0) > 0 || (app.draft?.binds.length ?? 0) > 0
    if (nonRollbackable && !window.confirm('部署/回滚不会回退 named volume 或 bind 内容。继续？')) return
    actionBusy = true; error = ''
    const request = {
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
      deployRetry = undefined
      window.location.hash = `/deployments/${result.deployment_id}`
    } catch { error = '部署 facts 已变化、Registry/Docker 不可用或已有部署正在运行。' } finally { actionBusy = false }
  }
  async function validateDraft() {
    actionBusy = true; error = ''
    try { validation = await mutation(`/api/v1/apps/${appId}/validate`, { draft: buildDraft() }); error = '' }
    catch { error = '配置预检失败，请检查资源、路径与 Compose 能力。' } finally { actionBusy = false }
  }
  async function saveDraft() {
    if (!app) return
    actionBusy = true; error = ''
    try {
      const request = { expected_revision: app.draft_revision, draft: buildDraft() }
      editRetry = retryIdentity(editRetry, request)
      await mutation(`/api/v1/apps/${appId}/draft`, request, { method: 'PUT', idempotencyKey: editRetry.key })
      clearSensitiveEnvironmentValues(editEnvironmentRows); editFileRows = editFileRows.map((row) => ({ ...row, value: row.sensitive ? '' : row.value })); editRetry = undefined
    } catch { error = '保存失败；可修正后重试，同一请求会复用幂等键。' } finally { actionBusy = false }
    if (error) return
    try { await load(); startEditing() }
    catch { error = '配置已保存，但刷新失败；请重新打开应用页面获取最新 revision。' }
  }
</script>

<main class="page-shell">
  <a class="back" href="#/">← 返回观察台</a>
  {#if error}<p class="notice danger">{error}</p>{/if}
  {#if app}
    <div class="detail-heading"><div><p class="eyebrow">APPLICATION</p><h1>{app.display_name}</h1><code>{app.id}</code></div><span class:healthy={app.actual?.health === 'healthy'} class="state-pill large">{app.deployment_status === 'UNCONFIGURED' ? '尚未配置' : app.deployment_status === 'DEPLOY_REQUIRED' ? '等待首次部署' : `${app.actual?.status ?? 'unavailable'} · ${app.actual?.health ?? 'unknown'}`}</span></div>
    <p class="notice"><strong>不可变 slug：</strong><code>{app.slug}</code> · <strong>Compose project：</strong><code>{app.resource_names.project_name}</code> · <strong>默认容器：</strong><code>{app.resource_names.project_name}-app-1</code></p>
    {#if app.expected_owned_default_network}<p class="notice"><strong>Owned network：</strong><code>{app.expected_owned_default_network.docker_name}</code> · <strong>Host bridge：</strong><code>{app.expected_owned_default_network.bridge_name}</code></p>{:else if !app.active_release && !app.pending_release_id && app.draft?.owned_default_network}<p class="notice"><strong>Draft owned network：</strong><code>{app.resource_names.owned_default_network_name}</code> · <strong>Host bridge：</strong><code>{app.resource_names.bridge_name}</code></p>{/if}
    <div class="actions"><button disabled={actionBusy || !app.available_actions.includes('deploy')} onclick={() => void deploy()}>部署 draft</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('start')} onclick={() => void lifecycle('start')}>启动</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('stop')} onclick={() => void lifecycle('stop')}>停止</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('restart')} onclick={() => void lifecycle('restart')}>重启</button><button class="danger danger-action" disabled={actionBusy} onclick={() => { deletionDialog = true; deletion = null; removeContainer = false }}>取消登记…</button></div>
    <div class="tabs"><button class:active={tab === 'overview'} onclick={() => { tab = 'overview' }}>概览</button><button class:active={tab === 'configuration'} onclick={() => { tab = 'configuration'; startEditing() }}>配置</button><button class:active={tab === 'deployments'} onclick={() => { tab = 'deployments' }}>部署历史</button><button class:active={tab === 'logs'} onclick={() => { tab = 'logs' }}>实时日志</button></div>
    {#if tab === 'logs'}
      <LogsPane {appId} />
    {:else if tab === 'deployments'}
      <section class="panel deployment-history" aria-label="部署历史">
        {#if deployments.length}
          <div class="deployment-row deployment-header" aria-hidden="true"><span>时间</span><span>状态 / 阶段</span><span>触发</span><span>镜像 / Digest</span><span>错误</span><span></span></div>
          {#each deployments as deployment}
            <article class="deployment-row">
              <time datetime={deployment.created_at}>{formatTimestamp(deployment.created_at, $timeSettings.timezone)}</time>
              <span><strong>{deployment.status}</strong><small>{deployment.phase}</small></span>
              <span>{deployment.trigger}</span>
              <code title={deployment.source_image_ref ?? deployment.manifest_digest ?? deployment.candidate_release_id ?? 'resolving'}>{deployment.source_image_ref ?? deployment.manifest_digest ?? deployment.candidate_release_id ?? 'resolving'}</code>
              <span>{deployment.error_code ?? '—'}</span>
              <a href={`#/deployments/${deployment.id}`}>查看详情</a>
            </article>
          {/each}
        {:else}<p class="muted">尚无部署历史。</p>{/if}
      </section>
    {:else if tab === 'configuration'}
      {#if editing}
        <form class="panel configuration-stack" onsubmit={(event) => { event.preventDefault(); void saveDraft() }}>
          <header><h2>Draft 配置</h2><p class="muted">Revision <code>{app.draft_revision ?? '尚未创建'}</code> · 保存会原子创建新的不可变 revision。</p></header>
          <label>Slug（不可修改）<input value={app.slug} readonly /></label><label>显示名称<input bind:value={editName} required /></label>
          <label>发现镜像 tag<input bind:value={editImage} required /></label><label>检查间隔（秒）<input type="number" min="60" max="86400" bind:value={editPoll} /></label>
          <label class="checkbox"><input type="checkbox" bind:checked={editAutoDeploy} /> 自动部署 tag 的新 digest</label>
          {#if editAutoDeploy}<p class="notice warning">启用后，新 digest 会自动替换容器并在健康失败时恢复旧 release；volume/bind 数据不会回滚。禁用不会取消已经 durable claim 的部署。</p>{/if}
          <label>Registry credential<select bind:value={editCredential}><option value={null}>匿名</option>{#each matchingCredentials as credential}<option value={credential.id}>{credential.registry} · {credential.username}</option>{/each}</select></label>
          <ImageSuggestions image={editImage} credentialRef={editCredential} bind:ports={editPorts} bind:volumes={editVolumes} />
          <EnvironmentEditor bind:rows={editEnvironmentRows} />
          <ManagedFileEditor bind:rows={editFileRows} />
          <PortEditor bind:ports={editPorts} />
          <StorageEditor bind:volumes={editVolumes} bind:binds={editBinds} {allowedBindRoots} />
          <NetworkEditor bind:ownedDefaultNetwork={editOwnedDefaultNetwork} bind:serviceDiscoveryEnabled={editServiceDiscovery} bind:externalNetworks={editNetworks} />
          <HealthLifecycleEditor bind:health={editHealth} bind:stopGrace={editStopGrace} />
          <p class="notice warning">读写 bind 必须显式设置 acknowledge_non_rollbackable；SoloDock 永不修改或删除其源目录。敏感输入只保留在当前表单内，成功后立即清空。</p>
          <div class="actions"><button type="button" class="ghost" disabled={actionBusy || !!editNetworkError} onclick={() => void validateDraft()}>仅预检</button><button disabled={actionBusy || !!editNetworkError}>保存新 revision</button><button type="button" class="ghost" onclick={() => { clearSensitiveEnvironmentValues(editEnvironmentRows); editFileRows = []; tab = 'overview' }}>取消</button></div>
          {#if validation}<article class="notice"><h3>Compose 预检</h3><p>{validation.plan.runnable ? '可运行' : '仅预览'} · 停机宽限 {validation.plan.stop_grace_period_seconds} 秒 · {validation.plan.ports} 端口 · {validation.plan.mounts} 挂载 · {validation.plan.networks} 网络 · {validation.plan.network_mode}</p>{#if validation.plan.owned_default_network}<p>Owned network：<code>{validation.plan.owned_default_network.docker_name}</code> · bridge：<code>{validation.plan.owned_default_network.bridge_name}</code></p>{/if}{#each validation.plan.external_networks as network}<p><code>{network.name}</code>{network.aliases.length ? ` · aliases: ${network.aliases.join(', ')}` : ''}</p>{/each}{#if validation.plan.external_networks.length}<p>External network 不由 SoloDock 创建、修改或删除。</p>{/if}{#each validation.plan.warnings as warning}<span class="tag">{warning}</span>{/each}<pre>{validation.compose_yaml}</pre></article>{/if}
        </form>
        {#if app.draft && webhook}<article class="panel webhook-panel"><h2>Registry recheck webhook</h2><p><span class="tag">{webhook.degraded ? '配置损坏：请生成新 secret 修复' : webhook.configured ? '已配置' : '未配置'}</span> · {webhook.algorithm}</p><p><code>{webhook.public_origin}{webhook.public_path}</code></p><p class="muted">Webhook 只触发一次 durable Registry recheck；不会信任 payload 中的镜像信息，也不会绕过自动部署、退避、drift 或健康门禁。</p>{#if webhookSecret}<p class="notice warning">这是 secret 唯一一次显示机会，请保存到 CI secret store：<code>{webhookSecret}</code></p><label class="checkbox"><input type="checkbox" bind:checked={webhookSaved} /> 我已安全保存该 secret</label><div class="actions"><button disabled={actionBusy || !webhookSaved} onclick={() => void saveWebhook()}>{webhook.configured ? '确认轮换' : '确认配置'}</button><button class="ghost" onclick={() => { webhookSecret = ''; webhookSaved = false; webhookRetry = undefined }}>取消</button></div>{:else}<div class="actions"><button disabled={actionBusy} onclick={prepareWebhookSecret}>{webhook.configured ? '生成轮换 secret' : '生成 webhook secret'}</button>{#if webhook.configured}<button class="danger" disabled={actionBusy} onclick={() => void revokeWebhook()}>撤销 webhook</button>{/if}</div>{/if}</article>{/if}
      {/if}
    {:else}
      {#if app.drift_codes.length}<div class="notice warning">{#each app.drift_codes as code}<span>{driftText(code)}</span>{/each}</div>{/if}
      <section class="detail-grid">
        <article class="panel"><h2>版本对照</h2><dl class="fact-list"><div><dt>活动镜像</dt><dd><code>{shortRef(app.active_release?.image_ref)}</code></dd></div><div><dt>实际镜像</dt><dd><code>{shortRef(app.actual?.configured_image_ref)}</code></dd></div><div><dt>容器 ID</dt><dd><code>{app.actual?.id.slice(0, 12) ?? '—'}</code></dd></div><div><dt>重启次数</dt><dd>{app.actual?.restart_count ?? '—'}</dd></div><div><dt>退出码</dt><dd>{app.actual?.exit_code ?? '—'}</dd></div></dl></article>
        <article class="panel"><h2>实时资源</h2><dl class="fact-list"><div><dt>CPU</dt><dd>{stats?.cpu_percent?.toFixed(2) ?? '—'}%</dd></div><div><dt>内存</dt><dd>{formatBytes(stats?.memory_usage_bytes ?? null)} / {formatBytes(stats?.memory_limit_bytes ?? null)}</dd></div><div><dt>接收</dt><dd>{formatBytes(stats?.network_rx_bytes ?? null)}</dd></div><div><dt>发送</dt><dd>{formatBytes(stats?.network_tx_bytes ?? null)}</dd></div></dl></article>
        <article class="panel wide"><h2>自动部署</h2><dl class="fact-list"><div><dt>状态</dt><dd>{app.draft?.auto_deploy_enabled ? '已启用' : '已禁用'}</dd></div><div><dt>最近结果</dt><dd class:warning={pollNeedsAttention(app.polling)}>{pollOutcomeText(app.polling)}</dd></div><div><dt>最近检查</dt><dd><time datetime={app.polling?.last_checked_at ?? undefined}>{formatTimestamp(app.polling?.last_checked_at, $timeSettings.timezone)}</time></dd></div><div><dt>下次不早于</dt><dd><time datetime={app.polling?.next_check_not_before ?? undefined}>{formatTimestamp(app.polling?.next_check_not_before, $timeSettings.timezone)}</time></dd></div><div><dt>Manifest</dt><dd><code>{app.polling?.last_manifest_digest ?? '—'}</code></dd></div><div><dt>平台</dt><dd>{app.polling?.last_platform ?? '—'}</dd></div><div><dt>错误</dt><dd>{app.polling?.last_error_code ?? '无'}</dd></div></dl>{#if app.polling?.suppressed_deployment_id}<a href={`#/deployments/${app.polling.suppressed_deployment_id}`}>查看被抑制的失败部署</a>{/if}</article>
        <article class="panel wide"><h2>端口</h2>{#each app.actual?.ports ?? [] as port}<p><code>{port.host_ip}:{port.host_port}</code> → {port.container_port}/{port.protocol}</p>{:else}<p class="muted">无 loopback 端口映射</p>{/each}</article>
        <article class="panel"><h2>挂载</h2>{#each app.actual?.mounts ?? [] as mount}<p><span class="tag">{mount.kind}</span> {mount.destination} · {mount.read_only ? '只读' : '读写'}</p>{:else}<p class="muted">无挂载</p>{/each}</article>
        <article class="panel wide"><h2>网络</h2><div class="network-comparison"><section><h3>期望</h3>{#if app.expected_network_plan}<p><span class="tag">{app.expected_network_plan.mode}</span></p>{#if app.expected_owned_default_network}<p><code>{app.expected_owned_default_network.docker_name}</code> · bridge <code>{app.expected_owned_default_network.bridge_name}</code></p>{/if}{#each app.expected_network_plan.external as network}<p><code>{network.name}</code>{network.aliases.length ? ` · aliases: ${network.aliases.join(', ')}` : ''}</p>{/each}{:else}<p class="muted">没有可关联的 immutable release 网络期望</p>{/if}</section><section><h3>实际</h3>{#if app.actual_owned_default_network}<p><code>{app.actual_owned_default_network.docker_name}</code> · {app.actual_owned_default_network.driver ?? 'unknown'} · bridge <code>{app.actual_owned_default_network.bridge_name ?? '未设置'}</code></p>{/if}{#each app.actual?.networks ?? [] as network}<p><code>{network.name}</code> · {network.container_ip ?? '—'}{network.aliases.length ? ` · DNS: ${network.aliases.join(', ')}` : ''}</p>{:else}<p class="muted">无容器网络 attachment</p>{/each}</section></div></article>
      </section>
    {/if}
  {/if}
  {#if deletionDialog}
    <div class="modal-backdrop" role="presentation"><div class="modal" role="dialog" aria-modal="true" aria-label="确认取消登记"><h2>确认取消登记</h2><p class="notice warning">默认只取消登记，容器、named volume、bind 内容和网络全部保留。{deletion?.orphan_warning ? '现有容器将成为 orphan。' : ''}</p><DeletionWebhookNotice configured={deletion?.webhook_configured ?? false} /><label class="checkbox"><input type="checkbox" bind:checked={removeContainer} disabled={deletion !== null} /> 同时移除精确 owned container（数据资源仍保留）</label>{#if deletion}<section class="deletion-preview"><p><strong>Compose project：</strong><code>{deletion.project_name}</code></p><p><strong>Active release：</strong><code>{deletion.active_release_id ?? '无'}</code></p><p><strong>Active config：</strong><code>{deletion.active_config_revision ?? '无'}</code></p><p><strong>Pending release：</strong><code>{deletion.pending_release_id ?? '无'}</code></p><p><strong>Pending config：</strong><code>{deletion.pending_config_revision ?? '无'}</code></p><p><strong>预览过期：</strong><time datetime={deletion.expires_at}>{formatTimestamp(deletion.expires_at, $timeSettings.timezone)}</time></p><p><strong>容器：</strong>{deletion.container_ids.length ? deletion.container_ids.join(', ') : '无'}</p><p><strong>托管文件：</strong>{deletion.managed_files.length ? deletion.managed_files.map((file) => `${file.logical_name} · ${file.configured_in}`).join(', ') : '无'}</p><p><strong>保留 owned volumes：</strong>{deletion.retained.owned_volumes.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || '无'}</p><p><strong>保留 external volumes：</strong>{deletion.retained.external_volumes.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || '无'}</p><p><strong>保留 bind：</strong>{deletion.retained.binds.map((bind) => `${retainedFact(bind.source, bind.configured_in, bind.exists)} (${bind.readonly ? 'ro' : 'rw'})`).join(', ') || '无'}</p><p><strong>保留网络：</strong>{deletion.retained.networks.map((item) => `${retainedFact(item.name, item.configured_in, item.exists)} · ${item.kind}${item.bridge_name ? ` · bridge: ${item.bridge_name}` : ''}${item.aliases.length ? ` · aliases: ${item.aliases.join(', ')}` : ''}`).join(', ') || '无'}</p></section><label>输入 <code>{deletion.slug}</code> 确认<input bind:value={confirmationSlug} autocomplete="off" /></label><div class="actions"><button class="danger" disabled={actionBusy || confirmationSlug !== deletion.slug || Date.parse(deletion.expires_at) <= Date.now()} onclick={() => void confirmDeletion()}>确认取消登记</button><button class="ghost" onclick={() => { deletion = null; deletionDialog = false; confirmationSlug = '' }}>取消</button></div>{:else}<div class="actions"><button class="danger" disabled={actionBusy} onclick={() => void previewDeletion()}>生成精确删除预览</button><button class="ghost" onclick={() => { deletionDialog = false }}>取消</button></div>{/if}</div></div>
  {/if}
</main>
