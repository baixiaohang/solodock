<script lang="ts">
  import { api, mutation } from '../lib/api'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import type { AppDetailResponse, AppMutationResponse } from '../lib/types'
  let slug = $state('postgres')
  let major = $state('18')
  let username = $state('postgres')
  let database = $state('postgres')
  let password = $state(generatePassword())
  let busy = $state(false); let error = $state(''); let copied = $state(false)
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
    busy = true; error = ''; copied = false
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
    } catch { error = createdAppId ? '服务和配置已创建，但尚未部署。可在此重试，或进入详情页继续部署。' : '创建失败；网络结果不明确时会复用同一密码和幂等键。' }
    finally { busy = false }
  }
</script>
<main class="page-shell narrow"><a class="back" href="#/apps/new">← 返回新建服务</a><div class="page-heading"><div><p class="eyebrow">QUICK DEPLOY</p><h1>PostgreSQL</h1><p class="muted">默认只需服务名；不会发布宿主端口，其他服务通过 <code>{slug || 'postgres'}:5432</code> 访问。</p></div></div>{#if error}<p class="notice danger" role="alert">{error}{#if createdAppId} <a href={`#/apps/${createdAppId}`}>进入服务详情</a>{/if}</p>{/if}<form class="panel configuration-stack" onsubmit={(event) => { event.preventDefault(); void create() }}><label>服务名<input bind:value={slug} maxlength="20" required disabled={createdAppId !== null} /></label><label>Major<select bind:value={major} disabled={createdAppId !== null}><option value="18">18（推荐）</option><option value="17">17</option></select></label><label>用户名<input bind:value={username} required disabled={createdAppId !== null} /></label><label>数据库<input bind:value={database} required disabled={createdAppId !== null} /></label>{#if !createdAppId}<label>自动生成密码<input type="password" bind:value={password} required minlength="16" /><span class="muted">创建前请复制保存；保存后 SoloDock 不会回显。</span></label>{/if}<div class="actions">{#if !createdAppId}<button type="button" class="ghost" onclick={() => { password = generatePassword(); copied = false }}>重新生成</button><button type="button" class="ghost" onclick={() => void copyPassword()}>{copied ? '已复制' : '复制密码'}</button>{/if}<button disabled={busy}>{busy ? '处理中…' : createdAppId ? '继续部署' : '创建并部署'}</button></div></form></main>
