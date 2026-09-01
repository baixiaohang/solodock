<script lang="ts">
  import { onMount } from 'svelte'
  import { auth, loadSession, logout, revokeAll } from './lib/auth'
  import AuthView from './components/AuthView.svelte'
  import AppShell from './components/AppShell.svelte'
  import Dashboard from './views/Dashboard.svelte'
  import AppDetail from './views/AppDetail.svelte'
  import NewApp from './views/NewApp.svelte'
  import Credentials from './views/Credentials.svelte'
  import DeploymentDetail from './views/DeploymentDetail.svelte'
  import Settings from './views/Settings.svelte'

  let route = window.location.hash
  const updateRoute = () => { route = window.location.hash }

  onMount(() => {
    void loadSession()
    window.addEventListener('hashchange', updateRoute)
    return () => window.removeEventListener('hashchange', updateRoute)
  })

  $: appId = route.match(/^#\/apps\/([0-9a-f-]+)$/)?.[1]
  $: creating = route === '#/apps/new'
  $: credentials = route === '#/credentials'
  $: settings = route === '#/settings'
  $: deploymentId = route.match(/^#\/deployments\/([0-9a-f-]+)$/)?.[1]
</script>

<svelte:head><title>SoloDock</title></svelte:head>

{#if $auth.kind === 'loading'}
  <main class="center-shell"><div class="spinner" aria-label="正在加载"></div></main>
{:else if $auth.kind === 'setup' || $auth.kind === 'login'}
  <AuthView mode={$auth.kind} />
{:else}
  <AppShell {route} onRevokeAll={revokeAll} onLogout={logout}>
    {#if settings}
      <Settings />
    {:else if credentials}
      <Credentials />
    {:else if deploymentId}
      <DeploymentDetail {deploymentId} />
    {:else if creating}
      <NewApp />
    {:else if appId}
      <AppDetail {appId} />
    {:else}
      <Dashboard />
    {/if}
  </AppShell>
{/if}
