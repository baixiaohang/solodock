<script lang="ts">
  import { ApiError } from '../lib/api'
  import { changePassword } from '../lib/auth'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { validatePassword } from '../lib/password'

  let currentPassword = $state('')
  let newPassword = $state('')
  let confirmation = $state('')
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)
  let requestId = $state('')

  async function submit() {
    if (busy) return
    error = null
    requestId = ''
    if (newPassword !== confirmation) {
      error = localized('The passwords do not match')
      return
    }
    const passwordError = validatePassword(newPassword)
    if (passwordError) {
      error = passwordError
      return
    }

    busy = true
    try {
      await changePassword(currentPassword, newPassword)
      currentPassword = ''
      newPassword = ''
      confirmation = ''
    } catch (cause) {
      if (!(cause instanceof ApiError) || cause.body.code === 'HTTP_ERROR') {
        error = localized('The password change result could not be confirmed. Reload and sign in with the new password before retrying.')
        if (cause instanceof ApiError) requestId = cause.body.request_id
      } else {
        requestId = cause.body.request_id
        if (cause.body.code === 'CURRENT_PASSWORD_INVALID') {
          error = localized('The current password is incorrect')
        } else if (cause.body.code === 'AUTH_COOLDOWN') {
          error = localized('Too many authentication attempts; try again later')
        } else {
          error = `${cause.body.code}: ${cause.body.message}`
        }
      }
    } finally {
      busy = false
    }
  }
</script>

<form class="settings-form" onsubmit={(event) => { event.preventDefault(); void submit() }}>
  <label>{$t('Current password')}<input type="password" bind:value={currentPassword} required autocomplete="current-password" /></label>
  <label>{$t('New password (14–128 characters)')}<input type="password" bind:value={newPassword} required minlength="14" autocomplete="new-password" /></label>
  <label>{$t('Confirm new password')}<input type="password" bind:value={confirmation} required minlength="14" autocomplete="new-password" /></label>
  {#if error}
    <p class="form-error" role="alert">
      {requestId
        ? $t('{detail} (request {requestId})', { detail: messageText(error, $t), requestId })
        : messageText(error, $t)}
    </p>
  {/if}
  <div class="actions"><button class="primary" disabled={busy}>{busy ? $t('Processing…') : $t('Change password')}</button></div>
  <p class="security-note">{$t('A successful change signs out every session. Sign in again with the new password.')}</p>
</form>
