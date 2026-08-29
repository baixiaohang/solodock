<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import type { AppDetailResponse, Deployment } from '../lib/types'
  import { isTerminalDeployment } from '../lib/deploymentState'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  let { deploymentId }: { deploymentId: string } = $props()
  let deployment = $state<Deployment | null>(null)
  let timer: ReturnType<typeof setTimeout> | undefined
  let error = $state('')
  let rollbackRetry = $state<RetryIdentity | undefined>()
  onMount(() => { void load(); return () => { if (timer) clearTimeout(timer) } })
  async function load() {
    try {
      deployment = await api(`/api/v1/deployments/${deploymentId}`)
      if (deployment && !isTerminalDeployment(deployment.status)) timer = setTimeout(() => void load(), 1000)
    } catch { error = '无法加载 deployment。' }
  }
  async function rollback() {
    if (!deployment || !window.confirm('回滚只切换 image/config；数据库、named volume 与 bind 内容不会回退。继续？')) return
    try {
      const app = await api<AppDetailResponse>(`/api/v1/apps/${deployment.app_id}`)
      const request = {
        expected_active_release_id: app.active_release?.id ?? null,
        expected_pending_release_id: app.pending_release_id,
        expected_actual_release_id: app.actual_release_id,
        expected_actual_container_id: app.actual?.id ?? null,
        acknowledge_non_rollbackable_data: true,
      }
      rollbackRetry = retryIdentity(rollbackRetry, request)
      const result = await mutation<{ deployment_id: string }>(`/api/v1/deployments/${deployment.id}/rollback`, request, { idempotencyKey: rollbackRetry.key })
      rollbackRetry = undefined
      window.location.hash = `/deployments/${result.deployment_id}`
    } catch { error = 'Rollback facts 已改变；请回到应用页刷新。' }
  }
</script>
<main class="page-shell narrow">
  <a class="back" href={deployment ? `#/apps/${deployment.app_id}` : '#/'}>← 返回应用</a>
  {#if error}<p class="notice danger">{error}</p>{/if}
  {#if deployment}
    <div class="page-heading"><div><p class="eyebrow">DEPLOYMENT</p><h1>{deployment.status}</h1><code>{deployment.id}</code></div><span class="state-pill large">{deployment.phase}</span></div>
    <article class="panel"><dl class="fact-list"><div><dt>触发</dt><dd>{deployment.trigger}</dd></div><div><dt>Source tag</dt><dd><code>{deployment.source_image_ref ?? '—'}</code></dd></div><div><dt>Manifest</dt><dd><code>{deployment.manifest_digest ?? '—'}</code></dd></div><div><dt>Platform</dt><dd>{deployment.platform ?? '—'}</dd></div><div><dt>Error</dt><dd><code>{deployment.error_code ?? '—'}</code></dd></div></dl></article>
    {#if deployment.warnings?.length}<p class="notice warning">{deployment.warnings.join(' · ')}</p>{/if}
    <article class="panel"><h2>当前事实</h2><dl class="fact-list"><div><dt>Safe release</dt><dd><code>{deployment.safe_release_id ?? '—'}</code></dd></div><div><dt>Active</dt><dd><code>{deployment.current_active_release_id ?? '—'}</code></dd></div><div><dt>Pending</dt><dd><code>{deployment.current_pending_release_id ?? '—'}</code></dd></div><div><dt>Actual</dt><dd><code>{deployment.current_actual_release_id ?? '—'}</code></dd></div></dl></article>
    <article class="panel"><h2>Timeline</h2>{#each deployment.transitions ?? [] as item}<p><code>{item.seq}</code> · {item.phase} · {item.result} {item.code ?? ''}</p>{/each}</article>
    {#if deployment.available_actions.includes('rollback')}<button class="ghost" onclick={() => void rollback()}>以此 release 回滚…</button>{/if}
  {/if}
</main>
