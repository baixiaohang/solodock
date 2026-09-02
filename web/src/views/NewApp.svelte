<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import type { AppMutationResponse, SettingsResponse } from '../lib/types'
  let slug = $state('')
  let busy = $state(false)
  let error = $state('')
  let retry = $state<RetryIdentity | undefined>()
  let slugMaxLength = $state(20)
  let slugValid = $derived(new RegExp(`^[a-z0-9](?:[a-z0-9-]{0,${Math.max(0, slugMaxLength - 2)}}[a-z0-9])?$`).test(slug))
  onMount(() => { void api<SettingsResponse>('/api/v1/settings').then((settings) => { slugMaxLength = settings.slug_max_length }).catch(() => undefined) })
  async function submit() {
    const request = { slug }; retry = retryIdentity(retry, request); busy = true; error = ''
    try { const result = await mutation<AppMutationResponse>('/api/v1/apps', request, { idempotencyKey: retry.key }); retry = undefined; window.location.hash = `/apps/${result.app.id}` }
    catch { error = '服务名无效、已存在，或创建结果不明确；重试会复用同一幂等键。' } finally { busy = false }
  }
</script>
<main class="page-shell narrow"><a class="back" href="#/">← 返回应用控制台</a><div class="page-heading"><div><p class="eyebrow">CREATE</p><h1>新建服务</h1><p class="muted">先登记服务名，创建后再按需配置和部署。</p></div></div>
{#if error}<p class="notice danger">{error}</p>{/if}
<form class="panel configuration-stack" onsubmit={(event) => { event.preventDefault(); void submit() }}><label>服务名<input bind:value={slug} required maxlength={slugMaxLength} pattern="[a-z0-9](?:[a-z0-9-]*[a-z0-9])?" placeholder="example-app" /><span class="muted">1–{slugMaxLength} 个小写字母、数字或连字符；创建后不可修改，同时作为内部 DNS 名称。</span></label><div class="actions"><button disabled={busy || !slugValid}>{busy ? '创建中…' : '创建空白服务'}</button></div></form>
<section class="panel"><h2>快速部署</h2><p class="muted">受版本控制的内置预设会生成普通 draft，不会执行任意 Compose。</p><a class="button-link" href="#/apps/new/postgresql">PostgreSQL</a></section></main>
