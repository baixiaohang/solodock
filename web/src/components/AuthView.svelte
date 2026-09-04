<script lang="ts">
  import { bootstrap, login } from '../lib/auth'
  import { ApiError } from '../lib/api'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { validatePassword } from '../lib/password'
  import LanguageSwitcher from './LanguageSwitcher.svelte'

  let { mode }: { mode: 'setup' | 'login' } = $props()
  let token = $state('')
  let password = $state('')
  let confirm = $state('')
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)

  async function submit() {
    error = null
    if (mode === 'setup' && password !== confirm) { error = localized('The passwords do not match'); return }
    const passwordError = validatePassword(password)
    if (passwordError) { error = passwordError; return }
    busy = true
    try {
      if (mode === 'setup') await bootstrap(token, password)
      else await login(password)
      token = ''; password = ''; confirm = ''
    } catch (cause) {
      error = cause instanceof ApiError ? authMessage(cause.body.code, cause.body.message) : localized('Could not connect to the control plane; try again later')
    } finally { busy = false }
  }

  function authMessage(code: string, message: string): UserMessage {
    if (code === 'BOOTSTRAP_TOKEN_INVALID') return localized('Bootstrap token is invalid')
    if (code === 'AUTH_INVALID') return localized('Incorrect password')
    if (code === 'AUTH_COOLDOWN') return localized('Too many sign-in attempts; try again later')
    return `${code}: ${message}`
  }
</script>

<main class="auth-shell">
  <div class="auth-language"><LanguageSwitcher /></div>
  <section class="auth-card">
    <div class="logo">SD</div>
    <p class="eyebrow">{$t('SOLODOCK CONTROL PLANE')}</p>
    <h1>{mode === 'setup' ? $t('Initialize administrator') : $t('Welcome back')}</h1>
    <p class="muted">{mode === 'setup' ? $t('Enter the one-time token from the server bootstrap.token file and set the administrator password.') : $t('Sign in to the single-administrator deployment console.')}</p>
    <form onsubmit={(event) => { event.preventDefault(); void submit() }}>
      {#if mode === 'setup'}
        <label>{$t('Bootstrap token')}<input type="password" bind:value={token} required autocomplete="off" spellcheck="false" /></label>
      {/if}
      <label>{$t('Password (14–128 characters)')}<input type="password" bind:value={password} required minlength="14" autocomplete={mode === 'login' ? 'current-password' : 'new-password'} /></label>
      {#if mode === 'setup'}
        <label>{$t('Confirm password')}<input type="password" bind:value={confirm} required minlength="14" autocomplete="new-password" /></label>
      {/if}
      {#if error}<p class="form-error" role="alert">{messageText(error, $t)}</p>{/if}
      <button class="primary" disabled={busy}>{busy ? $t('Processing…') : mode === 'setup' ? $t('Finish setup') : $t('Sign in')}</button>
    </form>
    <p class="security-note">{$t('Credentials are submitted only to the current origin and are never stored in browser storage.')}</p>
  </section>
</main>
