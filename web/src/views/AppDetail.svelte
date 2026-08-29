<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import { openSse } from '../lib/sse'
  import { driftText, formatBytes, shortRef } from '../lib/presentation'
  import { parseDotenv, serializeDotenv } from '../lib/dotenv'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { credentialsForReference } from '../lib/registryReference'
  import { canConfirmDeletion } from '../lib/deletionState'
  import { pollNeedsAttention, pollOutcomeText } from '../lib/pollingState'
  import type { AppDetailResponse, DeletionPreviewResponse, Deployment, DeploymentPage, DraftInput, RegistryCredential, StatsSample } from '../lib/types'
  import LogsPane from '../components/LogsPane.svelte'
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
  let validation = $state<{ plan: { warnings: string[]; runnable: boolean; ports: number; mounts: number; networks: number }; compose_yaml: string } | null>(null)
  let editSlug = $state('')
  let editName = $state('')
  let editImage = $state('')
  let editPoll = $state(300)
  let editAutoDeploy = $state(false)
  let editPublicEnv = $state('')
  let editSecretReplace = $state('')
  let editSecretDelete = $state('')
  let editPublicFiles = $state('[]')
  let editSecretFiles = $state('[]')
  let editPorts = $state('[]')
  let editVolumes = $state('[]')
  let editBinds = $state('[]')
  let editNetworks = $state('[]')
  let editHealth = $state('{"policy":"running","stable_window_seconds":15}')
  let credentials = $state<RegistryCredential[]>([])
  let editCredential = $state<string | null>(null)
  let deployments = $state<Deployment[]>([])
  let deployRetry = $state<RetryIdentity | undefined>()
  let matchingCredentials = $derived(credentialsForReference(credentials, editImage))
  $effect(() => {
    if (editCredential && credentials.length > 0 && !matchingCredentials.some((value) => value.id === editCredential)) editCredential = null
  })

  onMount(() => {
    void load()
    const source = openSse(`/api/v1/apps/${appId}/stats`, { stats: (event) => { stats = JSON.parse(event.data) as StatsSample } })
    return () => source.close()
  })

  async function load() {
    const [loadedApp, page, loadedCredentials] = await Promise.all([
      api<AppDetailResponse>(`/api/v1/apps/${appId}`),
      api<DeploymentPage>(`/api/v1/apps/${appId}/deployments?limit=20`),
      api<RegistryCredential[]>('/api/v1/registry-credentials'),
    ])
    app = loadedApp; deployments = page.items; credentials = loadedCredentials
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
    if (!app?.draft || !app.draft_revision) return
    editSlug = app.slug; editName = app.display_name; editImage = app.draft.discovery_image_ref
    editPoll = app.draft.poll_interval_seconds
    editAutoDeploy = app.draft.auto_deploy_enabled
    editPublicEnv = serializeDotenv(app.draft.public_environment)
    editSecretReplace = ''; editSecretDelete = ''
    editPublicFiles = pretty(app.draft.files.filter((file) => !file.sensitive).map((file) => ({ logical_name: file.logical_name, target_path: file.target_path, content: file.content ?? '' })))
    editSecretFiles = '[]'
    editPorts = pretty(app.draft.ports); editVolumes = pretty(app.draft.volumes)
    editBinds = pretty(app.draft.binds); editNetworks = pretty(app.draft.networks)
    editHealth = pretty(app.draft.health); editRetry = undefined; validation = null; editing = true
    editCredential = app.draft.credential_ref
  }
  function parseLines(value: string): Array<{ key: string; value: string }> {
    return parseDotenv(value)
  }
  function jsonArray(value: string): Array<Record<string, unknown>> {
    const parsed: unknown = JSON.parse(value); if (!Array.isArray(parsed)) throw new Error('array required')
    return parsed as Array<Record<string, unknown>>
  }
  function buildDraft(): DraftInput {
    if (!app?.draft) throw new Error('missing draft')
    const replacements = parseLines(editSecretReplace).map(({ key, value }) => ({ key, operation: 'replace' as const, value }))
    const deleted = new Set(editSecretDelete.split('\n').map((key) => key.trim()).filter(Boolean))
    const replacementKeys = new Set(replacements.map(({ key }) => key))
    const secretEnvironment: DraftInput['environment']['secrets'] = app.draft.secret_keys
      .filter((key) => !replacementKeys.has(key))
      .map((key) => deleted.has(key) ? { key, operation: 'delete' as const } : { key, operation: 'keep' as const })
    secretEnvironment.push(...replacements)
    const publicFiles = jsonArray(editPublicFiles).map((file) => ({
      logical_name: String(file.logical_name), target_path: String(file.target_path), sensitive: false as const,
      readonly: true as const, content: String(file.content ?? ''),
    }))
    const changedSecretNames = new Set<string>()
    const secretFiles = jsonArray(editSecretFiles).map((file) => {
      const logical_name = String(file.logical_name); const operation = String(file.operation)
      changedSecretNames.add(logical_name)
      if (operation === 'delete') return { logical_name, target_path: String(file.target_path), sensitive: true as const, readonly: true as const, operation: 'delete' as const }
      return { logical_name, target_path: String(file.target_path), sensitive: true as const, readonly: true as const, operation: 'replace' as const, value: String(file.value ?? '') }
    })
    const keptSecretFiles = app.draft.files.filter((file) => file.sensitive && !changedSecretNames.has(file.logical_name)).map((file) => ({
      logical_name: file.logical_name, target_path: file.target_path, sensitive: true as const, readonly: true as const, operation: 'keep' as const,
    }))
    return {
      slug: editSlug, display_name: editName, discovery_image_ref: editImage, credential_ref: editCredential,
      auto_deploy_enabled: editAutoDeploy, auto_deploy_acknowledged: editAutoDeploy && !app.draft.auto_deploy_enabled, poll_interval_seconds: editPoll,
      environment: { public: parseLines(editPublicEnv), secrets: secretEnvironment },
      files: [...publicFiles, ...keptSecretFiles, ...secretFiles],
      ports: jsonArray(editPorts) as unknown as DraftInput['ports'],
      volumes: jsonArray(editVolumes) as unknown as DraftInput['volumes'],
      binds: jsonArray(editBinds) as unknown as DraftInput['binds'],
      networks: jsonArray(editNetworks) as unknown as DraftInput['networks'],
      health: JSON.parse(editHealth) as DraftInput['health'],
    }
  }
  async function deploy() {
    if (!app?.draft_revision) return
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
    if (!app?.draft_revision) return
    actionBusy = true; error = ''
    try {
      const request = { expected_revision: app.draft_revision, draft: buildDraft() }
      editRetry = retryIdentity(editRetry, request)
      await mutation(`/api/v1/apps/${appId}/draft`, request, { method: 'PUT', idempotencyKey: editRetry.key })
      editSecretReplace = ''; editSecretFiles = '[]'; editRetry = undefined; editing = false; await load()
    } catch { error = '保存失败；可修正后重试，同一请求会复用幂等键。' } finally { actionBusy = false }
  }
</script>

<main class="page-shell">
  <a class="back" href="#/">← 返回观察台</a>
  {#if error}<p class="notice danger">{error}</p>{/if}
  {#if app}
    <div class="detail-heading"><div><p class="eyebrow">APPLICATION</p><h1>{app.display_name}</h1><code>{app.id}</code></div><span class:healthy={app.actual?.health === 'healthy'} class="state-pill large">{app.deployment_status === 'DEPLOY_REQUIRED' ? '等待首次部署' : `${app.actual?.status ?? 'unavailable'} · ${app.actual?.health ?? 'unknown'}`}</span></div>
    <div class="actions"><button disabled={actionBusy || !app.available_actions.includes('deploy')} onclick={() => void deploy()}>部署 draft</button><button disabled={actionBusy || !app.available_actions.includes('start')} onclick={() => void lifecycle('start')}>启动</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('stop')} onclick={() => void lifecycle('stop')}>停止</button><button class="ghost" disabled={actionBusy || !app.available_actions.includes('restart')} onclick={() => void lifecycle('restart')}>重启</button><button class="danger" disabled={actionBusy} onclick={() => { deletionDialog = true; deletion = null; removeContainer = false }}>取消登记…</button></div>
    {#if app.deployment_status === 'DEPLOY_REQUIRED'}<p class="notice warning">当前只有 draft config；点击“部署 draft”后才会把 tag 解析为本机平台 digest。</p>{:else if app.deployment_status === 'RUNNING'}<p class="notice warning">部署正在执行，期间只允许查看与安全删除预览。</p>{:else if app.deployment_status === 'PENDING'}<p class="notice warning">存在 pending release 或中断现场。请查看部署历史并基于当前 facts 明确重新收敛。</p>{/if}
    <div class="tabs"><button class:active={tab === 'overview'} onclick={() => { tab = 'overview' }}>概览</button><button class:active={tab === 'configuration'} onclick={() => { tab = 'configuration' }}>配置</button><button class:active={tab === 'deployments'} onclick={() => { tab = 'deployments' }}>部署历史</button><button class:active={tab === 'logs'} onclick={() => { tab = 'logs' }}>实时日志</button></div>
    {#if tab === 'logs'}
      <LogsPane {appId} />
    {:else if tab === 'deployments'}
      <section class="detail-grid">{#each deployments as deployment}<a class="panel" href={`#/deployments/${deployment.id}`}><h2>{deployment.status} · {deployment.phase}</h2><p><code>{deployment.source_image_ref ?? deployment.candidate_release_id ?? 'resolving'}</code></p><p class="muted">{deployment.created_at} · {deployment.error_code ?? '无错误'}</p></a>{:else}<article class="panel wide"><p class="muted">尚无部署历史。</p></article>{/each}</section>
    {:else if tab === 'configuration'}
      <section class="detail-grid">
        <article class="panel wide"><h2>不可变 draft revision</h2><dl class="fact-list"><div><dt>Revision</dt><dd><code>{app.draft_revision ?? '—'}</code></dd></div><div><dt>配置摘要</dt><dd><code>{app.draft_config_sha256?.slice(0, 16) ?? '—'}…</code></dd></div><div><dt>发现镜像</dt><dd><code>{app.draft?.discovery_image_ref ?? '—'}</code></dd></div><div><dt>期望状态</dt><dd>{app.desired_state}</dd></div></dl></article>
        <article class="panel"><h2>环境变量</h2>{#each app.draft?.public_environment ?? [] as entry}<p><code>{entry.key}</code> = {entry.value}</p>{/each}{#each app.draft?.secret_keys ?? [] as key}<p><code>{key}</code> · <span class="tag">write-only</span></p>{/each}</article>
        <article class="panel"><h2>资源</h2><p>{app.draft?.ports.length ?? 0} 个端口</p><p>{app.draft?.volumes.length ?? 0} 个 named volume</p><p>{app.draft?.binds.length ?? 0} 个受限 bind</p><p>{app.draft?.networks.length ?? 0} 个附加网络</p><p>{app.draft?.files.length ?? 0} 个托管文件</p></article>
        <article class="panel wide"><h2>编辑策略</h2><p class="muted">更新必须提交完整配置、携带当前 revision，并对每个既有 secret 显式选择 keep/replace/delete。Secret 内容永不回显。</p><button onclick={startEditing}>编辑完整配置…</button></article>
      </section>
      {#if editing}
        <form class="panel form-grid" onsubmit={(event) => { event.preventDefault(); void saveDraft() }}>
          <label>Slug<input bind:value={editSlug} required /></label><label>显示名称<input bind:value={editName} required /></label>
          <label class="wide">发现镜像 tag<input bind:value={editImage} required /></label><label>检查间隔（秒）<input type="number" min="60" max="86400" bind:value={editPoll} /></label>
          <label class="wide"><input type="checkbox" bind:checked={editAutoDeploy} /> 自动部署 tag 的新 digest</label>
          {#if editAutoDeploy}<p class="wide notice warning">启用后，新 digest 会自动替换容器并在健康失败时恢复旧 release；volume/bind 数据不会回滚。禁用不会取消已经 durable claim 的部署。</p>{/if}
          <label class="wide">Registry credential<select bind:value={editCredential}><option value={null}>匿名</option>{#each matchingCredentials as credential}<option value={credential.id}>{credential.registry} · {credential.username}</option>{/each}</select></label>
          <label class="wide">公开环境变量<textarea rows="5" bind:value={editPublicEnv}></textarea></label>
          <label>新增/替换 secret（KEY=value）<textarea rows="5" bind:value={editSecretReplace} autocomplete="off"></textarea></label>
          <label>删除 secret（每行一个 key）<textarea rows="5" bind:value={editSecretDelete}></textarea></label>
          <label class="wide">公开文件 JSON（logical_name、target_path、content）<textarea rows="6" bind:value={editPublicFiles}></textarea></label>
          <label class="wide">Secret 文件操作 JSON（logical_name、target_path、operation=replace/delete、value）<textarea rows="5" bind:value={editSecretFiles} autocomplete="off"></textarea></label>
          <label>端口 JSON<textarea rows="6" bind:value={editPorts}></textarea></label><label>Named volumes JSON<textarea rows="6" bind:value={editVolumes}></textarea></label>
          <label>受限 bind JSON<textarea rows="6" bind:value={editBinds}></textarea></label><label>网络 JSON<textarea rows="6" bind:value={editNetworks}></textarea></label>
          <label class="wide">健康策略 JSON<textarea rows="5" bind:value={editHealth}></textarea></label>
          <p class="wide notice warning">读写 bind 必须显式设置 acknowledge_non_rollbackable；SoloDock 永不修改或删除其源目录。敏感输入只保留在当前表单内，成功后立即清空。</p>
          <div class="wide actions"><button type="button" class="ghost" disabled={actionBusy} onclick={() => void validateDraft()}>仅预检</button><button disabled={actionBusy}>发布新 draft revision</button><button type="button" class="ghost" onclick={() => { editSecretReplace = ''; editSecretFiles = '[]'; editing = false }}>取消</button></div>
          {#if validation}<article class="wide notice"><h3>Compose 预检</h3><p>{validation.plan.runnable ? '可运行' : '仅预览'} · {validation.plan.ports} 端口 · {validation.plan.mounts} 挂载 · {validation.plan.networks} 网络</p>{#each validation.plan.warnings as warning}<span class="tag">{warning}</span>{/each}<pre>{validation.compose_yaml}</pre></article>{/if}
        </form>
      {/if}
    {:else}
      {#if app.drift_codes.length}<div class="notice warning">{#each app.drift_codes as code}<span>{driftText(code)}</span>{/each}</div>{/if}
      <section class="detail-grid">
        <article class="panel"><h2>版本对照</h2><dl class="fact-list"><div><dt>活动镜像</dt><dd><code>{shortRef(app.active_release?.image_ref)}</code></dd></div><div><dt>实际镜像</dt><dd><code>{shortRef(app.actual?.configured_image_ref)}</code></dd></div><div><dt>容器 ID</dt><dd><code>{app.actual?.id.slice(0, 12) ?? '—'}</code></dd></div><div><dt>重启次数</dt><dd>{app.actual?.restart_count ?? '—'}</dd></div><div><dt>退出码</dt><dd>{app.actual?.exit_code ?? '—'}</dd></div></dl></article>
        <article class="panel"><h2>实时资源</h2><dl class="fact-list"><div><dt>CPU</dt><dd>{stats?.cpu_percent?.toFixed(2) ?? '—'}%</dd></div><div><dt>内存</dt><dd>{formatBytes(stats?.memory_usage_bytes ?? null)} / {formatBytes(stats?.memory_limit_bytes ?? null)}</dd></div><div><dt>接收</dt><dd>{formatBytes(stats?.network_rx_bytes ?? null)}</dd></div><div><dt>发送</dt><dd>{formatBytes(stats?.network_tx_bytes ?? null)}</dd></div></dl></article>
        <article class="panel wide"><h2>自动部署</h2><dl class="fact-list"><div><dt>状态</dt><dd>{app.draft?.auto_deploy_enabled ? '已启用' : '已禁用'}</dd></div><div><dt>最近结果</dt><dd class:warning={pollNeedsAttention(app.polling)}>{pollOutcomeText(app.polling)}</dd></div><div><dt>最近检查</dt><dd>{app.polling?.last_checked_at ?? '—'}</dd></div><div><dt>下次不早于</dt><dd>{app.polling?.next_check_not_before ?? '—'}</dd></div><div><dt>Manifest</dt><dd><code>{app.polling?.last_manifest_digest ?? '—'}</code></dd></div><div><dt>平台</dt><dd>{app.polling?.last_platform ?? '—'}</dd></div><div><dt>错误</dt><dd>{app.polling?.last_error_code ?? '无'}</dd></div></dl>{#if app.polling?.suppressed_deployment_id}<a href={`#/deployments/${app.polling.suppressed_deployment_id}`}>查看被抑制的失败部署</a>{/if}</article>
        <article class="panel wide"><h2>端口</h2>{#each app.actual?.ports ?? [] as port}<p><code>{port.host_ip}:{port.host_port}</code> → {port.container_port}/{port.protocol}</p>{:else}<p class="muted">无 loopback 端口映射</p>{/each}</article>
        <article class="panel"><h2>挂载</h2>{#each app.actual?.mounts ?? [] as mount}<p><span class="tag">{mount.kind}</span> {mount.destination} · {mount.read_only ? '只读' : '读写'}</p>{:else}<p class="muted">无挂载</p>{/each}</article>
        <article class="panel"><h2>网络</h2>{#each app.actual?.networks ?? [] as network}<p>{network.name} · <code>{network.container_ip ?? '—'}</code></p>{:else}<p class="muted">无网络信息</p>{/each}</article>
      </section>
    {/if}
  {/if}
  {#if deletionDialog}
    <div class="modal-backdrop" role="presentation"><div class="modal" role="dialog" aria-modal="true" aria-label="确认取消登记"><h2>确认取消登记</h2><p class="notice warning">默认只取消登记，容器、named volume、bind 内容和网络全部保留。{deletion?.orphan_warning ? '现有容器将成为 orphan。' : ''}</p><label class="checkbox"><input type="checkbox" bind:checked={removeContainer} disabled={deletion !== null} /> 同时移除精确 owned container（数据资源仍保留）</label>{#if deletion}<section class="deletion-preview"><p><strong>Compose project：</strong><code>{deletion.project_name}</code></p><p><strong>Active release：</strong><code>{deletion.active_release_id ?? '无'}</code></p><p><strong>Active config：</strong><code>{deletion.active_config_revision ?? '无'}</code></p><p><strong>Pending release：</strong><code>{deletion.pending_release_id ?? '无'}</code></p><p><strong>Pending config：</strong><code>{deletion.pending_config_revision ?? '无'}</code></p><p><strong>预览过期：</strong>{deletion.expires_at}</p><p><strong>容器：</strong>{deletion.container_ids.length ? deletion.container_ids.join(', ') : '无'}</p><p><strong>托管文件：</strong>{deletion.managed_files.length ? deletion.managed_files.map((file) => `${file.logical_name} · ${file.configured_in}`).join(', ') : '无'}</p><p><strong>保留 owned volumes：</strong>{deletion.retained.owned_volumes.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || '无'}</p><p><strong>保留 external volumes：</strong>{deletion.retained.external_volumes.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || '无'}</p><p><strong>保留 bind：</strong>{deletion.retained.binds.map((bind) => `${retainedFact(bind.source, bind.configured_in, bind.exists)} (${bind.readonly ? 'ro' : 'rw'})`).join(', ') || '无'}</p><p><strong>保留网络：</strong>{deletion.retained.networks.map((item) => retainedFact(item.name, item.configured_in, item.exists)).join(', ') || '无'}</p></section><label>输入 <code>{deletion.slug}</code> 确认<input bind:value={confirmationSlug} autocomplete="off" /></label><div class="actions"><button class="danger" disabled={actionBusy || confirmationSlug !== deletion.slug || Date.parse(deletion.expires_at) <= Date.now()} onclick={() => void confirmDeletion()}>确认取消登记</button><button class="ghost" onclick={() => { deletion = null; deletionDialog = false; confirmationSlug = '' }}>取消</button></div>{:else}<div class="actions"><button class="danger" disabled={actionBusy} onclick={() => void previewDeletion()}>生成精确删除预览</button><button class="ghost" onclick={() => { deletionDialog = false }}>取消</button></div>{/if}</div></div>
  {/if}
</main>
