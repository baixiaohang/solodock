<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import type { RegistryCredential } from '../lib/types'
  import { clearWriteOnlyCredential, writeOnlyRetryIdentity } from '../lib/deploymentState'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  let credentials = $state<RegistryCredential[]>([])
  let registry = $state('')
  let username = $state('')
  let secret = $state('')
  let error = $state('')
  let busy = $state(false)
  let createRetry = $state<RetryIdentity | undefined>()
  let rotating = $state<RegistryCredential | null>(null)
  let rotateUsername = $state('')
  let rotateSecret = $state('')
  let rotateRetry = $state<RetryIdentity | undefined>()
  let deleteRetry = $state<RetryIdentity | undefined>()
  onMount(() => { void load() })
  async function load() { credentials = await api('/api/v1/registry-credentials') }
  async function create() {
    busy = true; error = ''
    const request = { registry, username, secret }
    createRetry = await writeOnlyRetryIdentity(createRetry, { registry, username }, secret)
    try {
      await mutation('/api/v1/registry-credentials', request, { idempotencyKey: createRetry.key })
      registry = ''; username = ''; createRetry = undefined; await load()
    } catch { error = 'Registry credential 创建失败；重新输入同一 token 可复用原请求身份。' } finally {
      const secretField = { secret }; clearWriteOnlyCredential(secretField); secret = secretField.secret; busy = false
    }
  }
  function beginRotate(item: RegistryCredential) {
    rotating = item; rotateUsername = item.username; rotateSecret = ''; rotateRetry = undefined
  }
  async function rotate() {
    if (!rotating || !rotateSecret) return
    busy = true; error = ''
    const request = { expected_revision: rotating.revision, username: rotateUsername, secret_operation: 'replace', secret: rotateSecret }
    rotateRetry = await writeOnlyRetryIdentity(rotateRetry, {
      credentialId: rotating.id, expected_revision: rotating.revision, username: rotateUsername, secret_operation: 'replace',
    }, rotateSecret)
    try {
      await mutation(`/api/v1/registry-credentials/${rotating.id}`, request, { method: 'PUT', idempotencyKey: rotateRetry.key })
      rotating = null; rotateRetry = undefined; await load()
    } catch { error = 'Credential 轮换失败；重新输入同一 token 可安全重试。' } finally {
      const secretField = { secret: rotateSecret }; clearWriteOnlyCredential(secretField); rotateSecret = secretField.secret; busy = false
    }
  }
  async function remove(item: RegistryCredential) {
    if (!window.confirm(`删除 ${item.registry} / ${item.username}？被应用或 release 引用时会被拒绝。`)) return
    busy = true; error = ''
    const request = { expected_revision: item.revision }
    deleteRetry = retryIdentity(deleteRetry, request)
    try {
      await mutation(`/api/v1/registry-credentials/${item.id}`, request, { method: 'DELETE', idempotencyKey: deleteRetry.key })
      deleteRetry = undefined; await load()
    } catch { error = 'Credential 正在使用或状态已改变。' } finally { busy = false }
  }
</script>

<main class="page-shell">
  <a class="back" href="#/">← 返回应用控制台</a>
  <div class="page-heading"><div><p class="eyebrow">REGISTRY</p><h1>Registry credentials</h1><p class="muted">Token 始终 write-only，只在 Registry resolve 与 operation-scoped pull 中使用。</p></div></div>
  {#if error}<p class="notice danger">{error}</p>{/if}
  <form class="panel form-grid" onsubmit={(event) => { event.preventDefault(); void create() }}>
    <label>Registry<input bind:value={registry} required placeholder="ghcr.io" /></label>
    <label>Username<input bind:value={username} required autocomplete="username" /></label>
    <label class="wide">Token / password<input type="password" bind:value={secret} required autocomplete="new-password" /></label>
    <div class="wide actions"><button disabled={busy}>保存 credential</button></div>
  </form>
  {#if rotating}
    <form class="panel form-grid" onsubmit={(event) => { event.preventDefault(); void rotate() }}>
      <h2 class="wide">轮换 {rotating.registry}</h2>
      <label>Username<input bind:value={rotateUsername} required autocomplete="username" /></label>
      <label>新 token / password<input type="password" bind:value={rotateSecret} required autocomplete="new-password" /></label>
      <div class="wide actions"><button disabled={busy}>确认轮换</button><button type="button" class="ghost" onclick={() => { rotateSecret = ''; rotateRetry = undefined; rotating = null }}>取消</button></div>
    </form>
  {/if}
  <section class="detail-grid">
    {#each credentials as item}
      <article class="panel"><h2>{item.registry}</h2><p><code>{item.username}</code></p><p class="muted">Revision {item.revision.slice(0, 8)} · {item.referenced_by_apps} 个 draft 引用</p><div class="actions"><button class="ghost" disabled={busy} onclick={() => beginRotate(item)}>轮换 secret</button><button class="danger" disabled={busy} onclick={() => void remove(item)}>删除</button></div></article>
    {:else}<article class="panel wide"><p class="muted">尚无 Registry credential；公开镜像可匿名部署。</p></article>{/each}
  </section>
</main>
