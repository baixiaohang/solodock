<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import type { RegistryCredential } from '../lib/types'
  import { clearWriteOnlyCredential, writeOnlyRetryIdentity } from '../lib/deploymentState'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  let credentials = $state<RegistryCredential[]>([])
  let registry = $state('')
  let username = $state('')
  let secret = $state('')
  let error = $state<UserMessage | null>(null)
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
    busy = true; error = null
    const request = { registry, username, secret }
    let created = false
    try {
      createRetry = await writeOnlyRetryIdentity(createRetry, { registry, username }, secret)
      await mutation('/api/v1/registry-credentials', request, { idempotencyKey: createRetry.key })
      created = true
      registry = ''; username = ''; createRetry = undefined
    } catch (cause) {
      const failure = mutationFailure(createRetry, cause)
      createRetry = failure.retry
      error = localized(failure.outcome === 'outcome_unknown'
        ? 'The secret mutation outcome could not be confirmed. Re-entering the same secret with unchanged fields will reuse its request identity.'
        : 'The secret mutation was not applied. Review the current state before re-entering it; the next attempt will use a new request identity.')
    } finally {
      const secretField = { secret }; clearWriteOnlyCredential(secretField); secret = secretField.secret
    }
    if (created) {
      try { await load() }
      catch { error = localized('The credential change succeeded, but refreshing the credential list failed. Reload the page to see the latest state.') }
    }
    busy = false
  }
  function beginRotate(item: RegistryCredential) {
    rotating = item; rotateUsername = item.username; rotateSecret = ''; rotateRetry = undefined
  }
  async function rotate() {
    if (!rotating || !rotateSecret) return
    busy = true; error = null
    const request = { expected_revision: rotating.revision, username: rotateUsername, secret_operation: 'replace', secret: rotateSecret }
    let rotated = false
    try {
      rotateRetry = await writeOnlyRetryIdentity(rotateRetry, {
        credentialId: rotating.id, expected_revision: rotating.revision, username: rotateUsername, secret_operation: 'replace',
      }, rotateSecret)
      await mutation(`/api/v1/registry-credentials/${rotating.id}`, request, { method: 'PUT', idempotencyKey: rotateRetry.key })
      rotated = true
      rotating = null; rotateRetry = undefined
    } catch (cause) {
      const failure = mutationFailure(rotateRetry, cause)
      rotateRetry = failure.retry
      error = localized(failure.outcome === 'outcome_unknown'
        ? 'The secret mutation outcome could not be confirmed. Re-entering the same secret with unchanged fields will reuse its request identity.'
        : 'The secret mutation was not applied. Review the current state before re-entering it; the next attempt will use a new request identity.')
    } finally {
      const secretField = { secret: rotateSecret }; clearWriteOnlyCredential(secretField); rotateSecret = secretField.secret
    }
    if (rotated) {
      try { await load() }
      catch { error = localized('The credential change succeeded, but refreshing the credential list failed. Reload the page to see the latest state.') }
    }
    busy = false
  }
  async function remove(item: RegistryCredential) {
    if (!window.confirm($t('Delete {registry} / {username}? The request will be rejected if an application or release still references it.', { registry: item.registry, username: item.username }))) return
    busy = true; error = null
    const request = { expected_revision: item.revision }
    deleteRetry = retryIdentity(deleteRetry, { credentialId: item.id, ...request })
    let removed = false
    try {
      await mutation(`/api/v1/registry-credentials/${item.id}`, request, { method: 'DELETE', idempotencyKey: deleteRetry.key })
      removed = true
      deleteRetry = undefined
    } catch (cause) {
      const failure = mutationFailure(deleteRetry, cause)
      deleteRetry = failure.retry
      error = localized(failure.outcome === 'outcome_unknown'
        ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
        : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
    }
    if (removed) {
      try { await load() }
      catch { error = localized('The credential change succeeded, but refreshing the credential list failed. Reload the page to see the latest state.') }
    }
    busy = false
  }
</script>

<main class="page-shell">
  <a class="back" href="#/">← {$t('Back to application console')}</a>
  <div class="page-heading"><div><p class="eyebrow">{$t('REGISTRY')}</p><h1>{$t('Registry credentials')}</h1><p class="muted">{$t('Registry tokens are always write-only and are used only for Registry resolution and operation-scoped pulls.')}</p></div></div>
  {#if error}<p class="notice danger">{messageText(error, $t)}</p>{/if}
  <form class="panel form-grid" onsubmit={(event) => { event.preventDefault(); void create() }}>
    <label>{$t('Registry')}<input bind:value={registry} required placeholder="ghcr.io" /></label>
    <label>{$t('Username')}<input bind:value={username} required autocomplete="username" /></label>
    <label class="wide">{$t('Token / password')}<input type="password" bind:value={secret} required autocomplete="new-password" /></label>
    <div class="wide actions"><button disabled={busy}>{$t('Save credential')}</button></div>
  </form>
  {#if rotating}
    <form class="panel form-grid" onsubmit={(event) => { event.preventDefault(); void rotate() }}>
      <h2 class="wide">{$t('Rotate {registry}', { registry: rotating.registry })}</h2>
      <label>{$t('Username')}<input bind:value={rotateUsername} required autocomplete="username" /></label>
      <label>{$t('New token / password')}<input type="password" bind:value={rotateSecret} required autocomplete="new-password" /></label>
      <div class="wide actions"><button disabled={busy}>{$t('Confirm rotation')}</button><button type="button" class="ghost" onclick={() => { rotateSecret = ''; rotateRetry = undefined; rotating = null }}>{$t('Cancel')}</button></div>
    </form>
  {/if}
  <section class="detail-grid">
    {#each credentials as item}
      <article class="panel"><h2>{item.registry}</h2><p><code>{item.username}</code></p><p class="muted">{$t('Revision')} {item.revision.slice(0, 8)} · {$t('Referenced by {count} drafts', { count: item.referenced_by_apps })}</p><div class="actions"><button class="ghost" disabled={busy} onclick={() => beginRotate(item)}>{$t('Rotate secret')}</button><button class="danger" disabled={busy} onclick={() => void remove(item)}>{$t('Delete')}</button></div></article>
    {:else}<article class="panel wide"><p class="muted">{$t('No Registry credentials. Public images can be deployed anonymously.')}</p></article>{/each}
  </section>
</main>
