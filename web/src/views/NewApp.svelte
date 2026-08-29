<script lang="ts">
  import { api, mutation } from '../lib/api'
  import { onMount } from 'svelte'
  import { parseDotenv } from '../lib/dotenv'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { credentialsForReference } from '../lib/registryReference'
  import type { AppMutationResponse, DraftInput, RegistryCredential } from '../lib/types'

  let slug = $state('')
  let displayName = $state('')
  let image = $state('')
  let publicEnv = $state('')
  let secretEnv = $state('')
  let pollInterval = $state(300)
  let autoDeploy = $state(false)
  let files = $state('[]')
  let ports = $state('[]')
  let volumes = $state('[]')
  let binds = $state('[]')
  let networks = $state('[{"kind":"owned_default"}]')
  let health = $state('{"policy":"running","stable_window_seconds":15}')
  let busy = $state(false)
  let error = $state('')
  let retry = $state<RetryIdentity | undefined>()
  let credentials = $state<RegistryCredential[]>([])
  let credentialRef = $state<string | null>(null)
  let matchingCredentials = $derived(credentialsForReference(credentials, image))
  $effect(() => {
    if (credentialRef && credentials.length > 0 && !matchingCredentials.some((value) => value.id === credentialRef)) credentialRef = null
  })
  onMount(() => { void api<RegistryCredential[]>('/api/v1/registry-credentials').then((value) => { credentials = value }) })

  function environment() {
    const publicEntries = parseDotenv(publicEnv)
    const secrets = parseDotenv(secretEnv).map(({ key, value }) => ({ key, operation: 'replace' as const, value }))
    return { public: publicEntries, secrets }
  }

  async function submit() {
    busy = true
    error = ''
    try {
      const draft: DraftInput = {
        slug, display_name: displayName, discovery_image_ref: image,
        credential_ref: credentialRef, auto_deploy_enabled: autoDeploy, auto_deploy_acknowledged: autoDeploy, poll_interval_seconds: pollInterval,
        environment: environment(), files: JSON.parse(files), ports: JSON.parse(ports),
        volumes: JSON.parse(volumes), binds: JSON.parse(binds), networks: JSON.parse(networks),
        health: JSON.parse(health),
      }
      retry = retryIdentity(retry, draft)
      const result = await mutation<AppMutationResponse>('/api/v1/apps', draft, { idempotencyKey: retry.key })
      secretEnv = ''
      retry = undefined
      window.location.hash = `/apps/${result.app.id}`
    } catch { error = '配置无效或注册失败。请检查字段后重试。' } finally { busy = false }
  }

  function handleSubmit(event: SubmitEvent) {
    event.preventDefault()
    submit().catch(() => { error = '注册失败。' })
  }
</script>

<main class="page-shell narrow">
  <a class="back" href="#/">← 返回应用控制台</a>
  <div class="page-heading"><div><p class="eyebrow">REGISTER</p><h1>注册应用</h1><p class="muted">这里只保存不可变 draft revision，不会拉取镜像或创建容器。</p></div></div>
  {#if error}<p class="notice danger">{error}</p>{/if}
  <form class="panel form-grid" onsubmit={handleSubmit}>
    <label>Slug<input bind:value={slug} required pattern="[a-z0-9-]+" maxlength="63" /></label>
    <label>显示名称<input bind:value={displayName} required maxlength="80" /></label>
    <label class="wide">发现镜像（必须带 tag）<input bind:value={image} required placeholder="registry.example/app:stable" /></label>
    <label class="wide">Registry credential<select bind:value={credentialRef}><option value={null}>匿名</option>{#each matchingCredentials as credential}<option value={credential.id}>{credential.registry} · {credential.username}</option>{/each}</select><span class="muted">只显示与镜像 logical registry 精确匹配的 credential。</span></label>
    <label>检查间隔（秒）<input type="number" min="60" max="86400" bind:value={pollInterval} /></label>
    <label class="wide"><input type="checkbox" bind:checked={autoDeploy} /> 自动部署 Registry tag 的新 digest</label>
    {#if autoDeploy}<div class="wide notice warning">新 digest 会自动替换容器，并在健康失败时恢复旧 release；volume 与 bind 中的数据不会随镜像回滚。</div>{/if}
    <label class="wide">公开环境变量<textarea bind:value={publicEnv} rows="5" placeholder="KEY=value&#10;OTHER=value"></textarea><span class="muted">有限 dotenv 语法；重复 key 会被拒绝。</span></label>
    <label class="wide">Secret 环境变量（可多项，KEY=value）<textarea bind:value={secretEnv} rows="5" autocomplete="new-password"></textarea><span class="muted">write-only；成功后立即清空。</span></label>
    <label class="wide">托管文件 JSON<textarea bind:value={files} rows="5"></textarea></label>
    <label>端口 JSON<textarea bind:value={ports} rows="5"></textarea></label>
    <label>Named volume JSON<textarea bind:value={volumes} rows="5"></textarea></label>
    <label>受限 bind JSON<textarea bind:value={binds} rows="5"></textarea></label>
    <label>网络 JSON<textarea bind:value={networks} rows="5"></textarea></label>
    <label class="wide">健康策略 JSON<textarea bind:value={health} rows="4"></textarea></label>
    <div class="wide notice warning">所有 M3 配置都随首个 immutable draft revision 原子保存；secret 为 write-only，不会在响应中返回。读写 bind 必须显式确认不可随 release 回滚。</div>
    <div class="wide actions"><button disabled={busy}>{busy ? '注册中…' : '注册应用'}</button><a class="ghost button-link" href="#/">取消</a></div>
  </form>
</main>
