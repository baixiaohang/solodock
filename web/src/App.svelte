<script lang="ts">
  import { onMount } from 'svelte'
  import { auth, loadSession, logout, revokeAll } from './lib/auth'
  import AuthView from './components/AuthView.svelte'
  import Dashboard from './views/Dashboard.svelte'
  import AppDetail from './views/AppDetail.svelte'

  let route = window.location.hash
  const updateRoute = () => { route = window.location.hash }

  onMount(() => {
    void loadSession()
    window.addEventListener('hashchange', updateRoute)
    return () => window.removeEventListener('hashchange', updateRoute)
  })

  $: appId = route.match(/^#\/apps\/([0-9a-f-]+)$/)?.[1]
</script>

<svelte:head><title>SoloDock</title></svelte:head>

{#if $auth.kind === 'loading'}
  <main class="center-shell"><div class="spinner" aria-label="正在加载"></div></main>
{:else if $auth.kind === 'setup' || $auth.kind === 'login'}
  <AuthView mode={$auth.kind} />
{:else}
  <header class="topbar">
    <a class="brand" href="#/">SoloDock <span>观察台</span></a>
    <nav aria-label="用户操作">
      <span class="user">admin</span>
      <button class="ghost" onclick={() => void revokeAll()}>撤销全部会话</button>
      <button class="ghost" onclick={() => void logout()}>退出</button>
    </nav>
  </header>
  {#if appId}
    <AppDetail {appId} />
  {:else}
    <Dashboard />
  {/if}
{/if}
