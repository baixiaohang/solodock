<script lang="ts">
  import { onMount } from 'svelte'
  import { mutation } from '../lib/api'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { applyTimeSettings, loadTimeSettings, timeSettings } from '../lib/time'
  import type { SettingsResponse } from '../lib/types'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  import PasswordChange from '../components/PasswordChange.svelte'

  let settings = $state<SettingsResponse | null>(null)
  let selected = $state('UTC')
  let bindRoots = $state<string[]>([])
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)
  let saved = $state(false)
  let retry = $state<RetryIdentity | undefined>()

  onMount(() => { void load() })

  async function load() {
    error = null
    try {
      settings = await loadTimeSettings()
      selected = settings.display_timezone
      bindRoots = [...(settings.allowed_bind_roots ?? [])]
      retry = undefined
    } catch {
      error = localized('Could not load global settings.')
    }
  }

  async function save() {
    if (!settings || !settings.supported_timezones.includes(selected)) return
    busy = true; error = null; saved = false
    const request = {
      expected_revision: settings.revision,
      display_timezone: selected,
      allowed_bind_roots: bindRoots.filter(Boolean),
    }
    retry = retryIdentity(retry, request)
    try {
      settings = await mutation<SettingsResponse>('/api/v1/settings', request, { method: 'PUT', idempotencyKey: retry.key })
      retry = undefined
      applyTimeSettings(settings)
      selected = settings.display_timezone
      bindRoots = [...(settings.allowed_bind_roots ?? [])]
      saved = true
    } catch {
      error = localized('Save failed. Settings may have changed in another page; refresh and try again.')
    } finally {
      busy = false
    }
  }

  function region(timezone: string): string {
    return timezone === 'UTC' ? $t('Common') : timezone.split('/')[0] ?? $t('Other')
  }
</script>

<main class="page-shell narrow">
  <div class="page-heading"><div><p class="eyebrow">{$t('SYSTEM')}</p><h1>{$t('System settings')}</h1><p class="muted">{$t('Manage display, storage access, and administrator security.')}</p></div></div>
  {#if error}<p class="notice danger" role="alert">{messageText(error, $t)}</p>{/if}
  {#if $timeSettings.unsupportedTimezone}<p class="notice warning" role="alert">{$t('This browser does not support {timezone}; times are temporarily displayed in UTC.', { timezone: $timeSettings.unsupportedTimezone })}</p>{/if}
  <div class="configuration-stack">
    {#if settings}
      <form class="panel settings-form" onsubmit={(event) => { event.preventDefault(); void save() }}>
        <label for="display-timezone">{$t('Display timezone')}</label>
        <select id="display-timezone" bind:value={selected} disabled={busy}>
          {#each settings.supported_timezones as timezone}
            <option value={timezone}>{timezone === 'UTC' ? $t('UTC (Coordinated Universal Time)') : `${timezone} · ${region(timezone)}`}</option>
          {/each}
        </select>
        <p class="muted">{$t('Options come from the backend\'s bundled IANA tzdb; arbitrary strings cannot be submitted. Saving redraws all open pages in the new timezone.')}</p>
        <fieldset class="row-editor"><legend>{$t('Storage access')}</legend><p class="muted">{$t('One existing, safe host directory per line. SoloDock uses these only as the bind allowlist and never browses, creates, changes permissions on, or deletes them.')}</p>{#each bindRoots as root, index}<div class="editor-row"><label>{$t('Allowed root')}<input bind:value={bindRoots[index]} placeholder="/home/ubuntu" required /></label><button type="button" class="ghost" onclick={() => { bindRoots = bindRoots.filter((_, current) => current !== index) }}>{$t('Delete')}</button></div>{/each}<button type="button" class="ghost" onclick={() => { bindRoots = [...bindRoots, ''] }}>{$t('Add root')}</button></fieldset>
        <div class="actions"><button disabled={busy}>{busy ? $t('Saving…') : $t('Save system settings')}</button>{#if saved}<span class="success-text" role="status">{$t('Applied')}</span>{/if}</div>
      </form>
    {/if}
    <section class="panel">
      <h2>{$t('Administrator security')}</h2>
      <p class="muted">{$t('Change the single administrator password and revoke every active session atomically.')}</p>
      <PasswordChange />
    </section>
  </div>
</main>
