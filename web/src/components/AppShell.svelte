<script lang="ts">
  import { onMount } from 'svelte'
  import type { Snippet } from 'svelte'
  import { loadTimeSettings, timeSettings } from '../lib/time'

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
        aria-label="打开主导航"
        aria-expanded={menuOpen}
        aria-controls="primary-navigation"
        onclick={() => { menuOpen = !menuOpen }}
      >
        <span aria-hidden="true">☰</span>
      </button>
      <a class="brand" href="#/" onclick={() => closeMenu()}>SoloDock <span>Control Panel</span></a>
    </div>
    <nav class="user-actions" aria-label="用户操作">
      <span class="user">admin</span>
      <button class="topbar-action" type="button" onclick={() => void onRevokeAll()}>撤销全部会话</button>
      <button class="topbar-action" type="button" onclick={() => void onLogout()}>退出</button>
    </nav>
  </header>

  <div class="shell-body">
    <aside
      id="primary-navigation"
      class:open={menuOpen}
      class="sidebar"
      aria-label="主导航"
      aria-hidden={mobile && !menuOpen ? 'true' : undefined}
      inert={mobile && !menuOpen}
    >
      <div class="sidebar-heading">
        <span>控制台</span>
        <button class="drawer-close" type="button" aria-label="关闭主导航" onclick={() => closeMenu(true)}>×</button>
      </div>
      <nav>
        <a class:active={applicationsActive} aria-current={applicationsActive ? 'page' : undefined} href="#/" onclick={() => closeMenu()}>
          <span class="nav-icon" aria-hidden="true">▦</span>
          应用
        </a>
        <a class:active={credentialsActive} aria-current={credentialsActive ? 'page' : undefined} href="#/credentials" onclick={() => closeMenu()}>
          <span class="nav-icon" aria-hidden="true">⌾</span>
          Registry 凭据
        </a>
        <a class:active={settingsActive} aria-current={settingsActive ? 'page' : undefined} href="#/settings" onclick={() => closeMenu()}>
          <span class="nav-icon" aria-hidden="true">◷</span>
          系统设置
        </a>
      </nav>
    </aside>
    {#if menuOpen}
      <button class="drawer-backdrop" type="button" aria-label="关闭主导航" onclick={() => closeMenu(true)}></button>
    {/if}
    <div class="content-canvas">
      {#if $timeSettings.warning}<p class="notice warning global-warning" role="alert">{$timeSettings.warning}</p>{/if}
      {#if children}{@render children()}{/if}
    </div>
  </div>
</div>
