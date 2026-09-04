<script lang="ts">
  import { onMount } from 'svelte'
  import { api, mutation } from '../lib/api'
  import { mutationFailure, retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import type { AppMutationResponse, SettingsResponse } from '../lib/types'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  let slug = $state('')
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)
  let retry = $state<RetryIdentity | undefined>()
  let slugMaxLength = $state(20)
  let slugValid = $derived(new RegExp(`^[a-z0-9](?:[a-z0-9-]{0,${Math.max(0, slugMaxLength - 2)}}[a-z0-9])?$`).test(slug))
  onMount(() => { void api<SettingsResponse>('/api/v1/settings').then((settings) => { slugMaxLength = settings.slug_max_length }).catch(() => undefined) })
  async function submit() {
    const request = { slug }; retry = retryIdentity(retry, request); busy = true; error = null
    try { const result = await mutation<AppMutationResponse>('/api/v1/apps', request, { idempotencyKey: retry.key }); retry = undefined; window.location.hash = `/apps/${result.app.id}` }
    catch (cause) {
      const failure = mutationFailure(retry, cause)
      retry = failure.retry
      error = localized(failure.outcome === 'outcome_unknown'
        ? 'The request outcome could not be confirmed. Retrying the same unchanged request will reuse its idempotency key.'
        : 'The request was not applied. Review the current state before trying again; the next attempt will use a new idempotency key.')
    } finally { busy = false }
  }
</script>
<main class="page-shell narrow"><a class="back" href="#/">← {$t('Back to application console')}</a><div class="page-heading"><div><p class="eyebrow">{$t('CREATE')}</p><h1>{$t('New service')}</h1><p class="muted">{$t('Register the service name first, then configure and deploy it.')}</p></div></div>
{#if error}<p class="notice danger">{messageText(error, $t)}</p>{/if}
<form class="panel configuration-stack" onsubmit={(event) => { event.preventDefault(); void submit() }}><label>{$t('Service name')}<input bind:value={slug} required maxlength={slugMaxLength} pattern="[a-z0-9](?:[a-z0-9-]*[a-z0-9])?" placeholder="example-app" /><span class="muted">{$t('Use 1–{max} lowercase letters, digits, or hyphens. The name is immutable and is also used for internal DNS.', { max: slugMaxLength })}</span></label><div class="actions"><button disabled={busy || !slugValid}>{busy ? $t('Creating…') : $t('Create blank service')}</button></div></form>
<section class="panel"><h2>{$t('Quick deploy')}</h2><p class="muted">{$t('Versioned built-in presets create regular drafts and never run arbitrary Compose.')}</p><a class="button-link" href="#/apps/new/postgresql">PostgreSQL</a></section></main>
