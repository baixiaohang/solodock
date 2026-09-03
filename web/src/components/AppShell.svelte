<script lang="ts">
  import { onMount } from 'svelte'
  import type { Snippet } from 'svelte'
  import { loadTimeSettings, timeSettings } from '../lib/time'
  import { t } from '../lib/i18n'
  import LanguageSwitcher from './LanguageSwitcher.svelte'

  let {
    route,
    onRevokeAll,
    onLogout,
    children,
  }: {
    route: string
    onRevokeAll: () => void | Promise<void>
    onLogout: () => void | Promise<void>
    children?: Snippet
  } = $props()

  let menuOpen = $state(false)
  let mobile = $state(false)
  let menuButton = $state<HTMLButtonElement>()
  let applicationsActive = $derived(route === '' || route === '#/' || /^#\/(apps|deployments)(\/|$)/.test(route))
  let credentialsActive = $derived(route === '#/credentials')
  let settingsActive = $derived(route === '#/settings')

  onMount(() => {
    void loadTimeSettings().catch(() => {})
    const media = window.matchMedia('(max-width: 800px)')
    const updateMobile = () => {
      mobile = media.matches
      if (!mobile) menuOpen = false
    }
    updateMobile()
    media.addEventListener('change', updateMobile)
    return () => media.removeEventListener('change', updateMobile)
  })

  function closeMenu(restoreFocus = false) {
    menuOpen = false
    if (restoreFocus && mobile) menuButton?.focus()
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && menuOpen) closeMenu(true)
  }
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="app-shell">
  <header class="topbar">
    <div class="topbar-brand-group">
      <button
        bind:this={menuButton}
        class="menu-toggle"
        type="button"
        aria-label={$t('Open primary navigation')}
        aria-expanded={menuOpen}
        aria-controls="primary-navigation"
        onclick={() => { menuOpen = !menuOpen }}
      >
        <span aria-hidden="true">☰</span>
      </button>
      <a class="brand" href="#/" onclick={() => closeMenu()}>SoloDock <span>{$t('Control Panel')}</span></a>
    </div>
    <nav class="user-actions" aria-label={$t('User actions')}>
      <LanguageSwitcher />
      <span class="user">admin</span>
      <button class="topbar-action" type="button" onclick={() => void onRevokeAll()}>{$t('Revoke all sessions')}</button>
      <button class="topbar-action" type="button" onclick={() => void onLogout()}>{$t('Log out')}</button>
    </nav>
  </header>

  <div class="shell-body">
    <aside
      id="primary-navigation"
      class:open={menuOpen}
      class="sidebar"
      aria-label={$t('Primary navigation')}
      aria-hidden={mobile && !menuOpen ? 'true' : undefined}
      inert={mobile && !menuOpen}
    >
      <div class="sidebar-heading">
        <span>{$t('Console')}</span>
        <button class="drawer-close" type="button" aria-label={$t('Close primary navigation')} onclick={() => closeMenu(true)}>×</button>
      </div>
      <div class="sidebar-language"><LanguageSwitcher /></div>
      <nav>
        <a class:active={applicationsActive} aria-current={applicationsActive ? 'page' : undefined} href="#/" onclick={() => closeMenu()}>
          <span class="nav-icon" aria-hidden="true">▦</span>
          {$t('Applications')}
        </a>
        <a class:active={credentialsActive} aria-current={credentialsActive ? 'page' : undefined} href="#/credentials" onclick={() => closeMenu()}>
          <span class="nav-icon" aria-hidden="true">⌾</span>
          {$t('Registry credentials')}
        </a>
        <a class:active={settingsActive} aria-current={settingsActive ? 'page' : undefined} href="#/settings" onclick={() => closeMenu()}>
          <span class="nav-icon" aria-hidden="true">◷</span>
          {$t('System settings')}
        </a>
      </nav>
    </aside>
    {#if menuOpen}
      <button class="drawer-backdrop" type="button" aria-label={$t('Close primary navigation')} onclick={() => closeMenu(true)}></button>
    {/if}
    <div class="content-canvas">
      {#if $timeSettings.unsupportedTimezone}<p class="notice warning global-warning" role="alert">{$t('This browser does not support {timezone}; times are temporarily displayed in UTC.', { timezone: $timeSettings.unsupportedTimezone })}</p>{/if}
      {#if children}{@render children()}{/if}
    </div>
  </div>
</div>
