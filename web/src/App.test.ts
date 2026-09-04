// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import App from './App.svelte'
import { auth } from './lib/auth'

class MockEventSource {
  static instances: MockEventSource[] = []
  closed = false

  constructor(public url: string) {
    MockEventSource.instances.push(this)
  }

  addEventListener() {}
  close() { this.closed = true }
}

const settings = {
  revision: '00000000-0000-4000-8000-000000000099', display_timezone: 'UTC', supported_timezones: ['UTC'],
  allowed_bind_roots: ['/srv/solodock-data'], slug_max_length: 20,
  supported_mount_types: ['owned_volume', 'external_volume', 'bind'],
  configuration_limits: { health: {
    running_stable_window_seconds: { min: 5, max: 300, default: 15 },
    http_interval_seconds: { min: 1, max: 300, default: 10 },
    http_timeout_seconds: { min: 1, max: 60, default: 5 },
    http_retries: { min: 1, max: 10, default: 6 },
    http_start_period_seconds: { min: 0, max: 300, default: 30 },
    stop_grace_period_seconds: { min: 1, max: 600, default: 10 },
  } },
}

function appDetail(id: string, name: string) {
  return {
    id, slug: name.toLowerCase(), display_name: name,
    resource_names: { project_name: `solodock-${name.toLowerCase()}`, owned_default_network_name: `solodock-${name.toLowerCase()}-default`, bridge_name: 'sd-0123456789ab' },
    active_release: null, actual_release_id: null, actual: null, expected_network_plan: null,
    expected_owned_default_network: null, actual_owned_default_network: null, drift_codes: [], draft: null,
    draft_revision: null, draft_config_sha256: null, active_config_revision: null, pending_release_id: null,
    pending_image_ref: null, desired_state: 'stopped', deployment_status: 'UNCONFIGURED',
    available_actions: ['start', 'deletion_preview'], compose_available: true, polling: null,
  }
}

function configuredApp(id: string, name: string) {
  return {
    ...appDetail(id, name),
    draft_revision: '00000000-0000-4000-8000-000000000090',
    deployment_status: 'DEPLOY_REQUIRED',
    available_actions: ['deploy', 'deletion_preview'],
    draft: {
      display_name: name, discovery_image_ref: 'registry.example/app:stable', credential_ref: null,
      auto_deploy_enabled: false, poll_interval_seconds: 300, stop_grace_period_seconds: 10,
      public_environment: [], secret_keys: [], files: [], ports: [], volumes: [], binds: [],
      owned_default_network: true, service_discovery_enabled: true, networks: [],
      health: { policy: 'running', stable_window_seconds: 15 },
    },
  }
}

afterEach(() => {
  cleanup()
  MockEventSource.instances = []
  auth.set({ kind: 'loading' })
  window.location.hash = ''
  vi.unstubAllGlobals()
})

describe('route resource identity', () => {
  it('remounts app details so a late A response cannot target B', async () => {
    const appA = appDetail('00000000-0000-4000-8000-000000000001', 'App A')
    const appB = appDetail('00000000-0000-4000-8000-000000000002', 'App B')
    let resolveA: ((response: Response) => void) | undefined
    const lateA = new Promise<Response>((resolve) => { resolveA = resolve })
    const mutationUrls: string[] = []

    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener() {}, removeEventListener() {} })))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/api/v1/me')) return new Response(JSON.stringify({ username: 'admin', session: { created_at: '2026-01-01T00:00:00Z', expires_at: '2026-01-02T00:00:00Z' } }), { status: 200 })
      if (url.endsWith('/api/v1/system/installation')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith(`/apps/${appA.id}`)) return lateA
      if (url.endsWith(`/apps/${appB.id}/actions/start`) && init?.method === 'POST') {
        mutationUrls.push(url)
        return new Response('{}', { status: 200 })
      }
      if (url.endsWith(`/apps/${appB.id}`)) return new Response(JSON.stringify(appB), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    window.location.hash = `#/apps/${appA.id}`
    auth.set({ kind: 'authenticated', me: { username: 'admin', session: { created_at: '2026-01-01T00:00:00Z', expires_at: '2026-01-02T00:00:00Z' } } })
    render(App)
    await waitFor(() => expect(MockEventSource.instances[0]?.url).toContain(appA.id))

    window.location.hash = `#/apps/${appB.id}`
    window.dispatchEvent(new HashChangeEvent('hashchange'))
    expect(await screen.findByText('App B')).toBeTruthy()
    expect(MockEventSource.instances[0].closed).toBe(true)
    expect(MockEventSource.instances[1].url).toContain(appB.id)

    resolveA?.(new Response(JSON.stringify(appA), { status: 200 }))
    await Promise.resolve()
    expect(screen.queryByText('App A')).toBeNull()

    await userEvent.setup().click(screen.getByRole('button', { name: '启动' }))
    await waitFor(() => expect(mutationUrls).toEqual([`/api/v1/apps/${appB.id}/actions/start`]))
  })

  it('remounts deployment details so polling and rollback stay bound to B', async () => {
    const deploymentA = '00000000-0000-4000-8000-000000000011'
    const deploymentB = '00000000-0000-4000-8000-000000000012'
    const appB = appDetail('00000000-0000-4000-8000-000000000022', 'App B')
    let resolveA: ((response: Response) => void) | undefined
    const lateA = new Promise<Response>((resolve) => { resolveA = resolve })
    const requestedDeployments: string[] = []
    const mutationUrls: string[] = []

    vi.stubGlobal('confirm', vi.fn(() => true))
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener() {}, removeEventListener() {} })))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/api/v1/me')) return new Response(JSON.stringify({ username: 'admin', session: { created_at: '2026-01-01T00:00:00Z', expires_at: '2026-01-02T00:00:00Z' } }), { status: 200 })
      if (url.endsWith('/api/v1/system/installation')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith(`/deployments/${deploymentA}`)) {
        requestedDeployments.push(deploymentA)
        return lateA
      }
      if (url.endsWith(`/deployments/${deploymentB}/rollback`) && init?.method === 'POST') {
        mutationUrls.push(url)
        return new Response(JSON.stringify({ deployment_id: deploymentB }), { status: 202 })
      }
      if (url.endsWith(`/deployments/${deploymentB}`)) {
        requestedDeployments.push(deploymentB)
        return new Response(JSON.stringify({
          id: deploymentB,
          app_id: appB.id,
          status: 'failed',
          phase: 'terminal',
          trigger: 'manual',
          transitions: [],
          warnings: [],
          available_actions: ['rollback'],
        }), { status: 200 })
      }
      if (url.endsWith(`/apps/${appB.id}`)) return new Response(JSON.stringify(appB), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    window.location.hash = `#/deployments/${deploymentA}`
    auth.set({ kind: 'authenticated', me: { username: 'admin', session: { created_at: '2026-01-01T00:00:00Z', expires_at: '2026-01-02T00:00:00Z' } } })
    render(App)
    await waitFor(() => expect(requestedDeployments).toEqual([deploymentA]))

    window.location.hash = `#/deployments/${deploymentB}`
    window.dispatchEvent(new HashChangeEvent('hashchange'))
    expect(await screen.findByText(deploymentB)).toBeTruthy()

    resolveA?.(new Response(JSON.stringify({
      id: deploymentA,
      app_id: '00000000-0000-4000-8000-000000000021',
      status: 'failed',
      phase: 'terminal',
      trigger: 'manual',
      transitions: [],
      warnings: [],
      available_actions: ['rollback'],
    }), { status: 200 }))
    await Promise.resolve()
    expect(screen.queryByText(deploymentA)).toBeNull()

    await userEvent.setup().click(screen.getByRole('button', { name: /Roll back|回滚/ }))
    await waitFor(() => expect(mutationUrls).toEqual([`/api/v1/deployments/${deploymentB}/rollback`]))
    expect(requestedDeployments).toEqual([deploymentA, deploymentB])
  })

  it('does not navigate when an A deploy response arrives after switching to B', async () => {
    const appA = configuredApp('00000000-0000-4000-8000-000000000031', 'App A')
    const appB = appDetail('00000000-0000-4000-8000-000000000032', 'App B')
    let resolveDeploy: ((response: Response) => void) | undefined
    const pendingDeploy = new Promise<Response>((resolve) => { resolveDeploy = resolve })

    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener() {}, removeEventListener() {} })))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/api/v1/me')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/system/installation')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith(`/apps/${appA.id}/deployments`) && init?.method === 'POST') return pendingDeploy
      if (url.endsWith(`/apps/${appA.id}`)) return new Response(JSON.stringify(appA), { status: 200 })
      if (url.endsWith(`/apps/${appB.id}`)) return new Response(JSON.stringify(appB), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    window.location.hash = `#/apps/${appA.id}`
    auth.set({ kind: 'authenticated', me: { username: 'admin', session: { created_at: '', expires_at: '' } } })
    render(App)
    expect(await screen.findByText('App A')).toBeTruthy()
    await userEvent.setup().click(screen.getByRole('button', { name: /Deploy draft|部署 draft/ }))

    window.location.hash = `#/apps/${appB.id}`
    window.dispatchEvent(new HashChangeEvent('hashchange'))
    expect(await screen.findByText('App B')).toBeTruthy()
    resolveDeploy?.(new Response(JSON.stringify({ deployment_id: '00000000-0000-4000-8000-000000000039' }), { status: 202 }))
    await Promise.resolve()
    expect(window.location.hash).toBe(`#/apps/${appB.id}`)
  })

  it('does not navigate when an A delete response arrives after switching to B', async () => {
    const appA = appDetail('00000000-0000-4000-8000-000000000041', 'App A')
    const appB = appDetail('00000000-0000-4000-8000-000000000042', 'App B')
    let resolveDelete: ((response: Response) => void) | undefined
    const pendingDelete = new Promise<Response>((resolve) => { resolveDelete = resolve })

    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener() {}, removeEventListener() {} })))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/api/v1/me')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/system/installation')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith(`/apps/${appA.id}/deletion-preview`)) return new Response(JSON.stringify({
        confirmation_token: 'token', slug: appA.slug, expected_revision: 'revision', expires_at: '2099-01-01T00:00:00Z',
        project_name: appA.resource_names.project_name, active_release_id: null, active_config_revision: null,
        pending_release_id: null, pending_config_revision: null, container_ids: [], managed_files: [],
        webhook_configured: false, orphan_warning: false,
        retained: { owned_volumes: [], external_volumes: [], binds: [], networks: [] },
      }), { status: 200 })
      if (url.endsWith(`/apps/${appA.id}`) && init?.method === 'DELETE') return pendingDelete
      if (url.endsWith(`/apps/${appA.id}`)) return new Response(JSON.stringify(appA), { status: 200 })
      if (url.endsWith(`/apps/${appB.id}`)) return new Response(JSON.stringify(appB), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    window.location.hash = `#/apps/${appA.id}`
    auth.set({ kind: 'authenticated', me: { username: 'admin', session: { created_at: '', expires_at: '' } } })
    render(App)
    expect(await screen.findByText('App A')).toBeTruthy()
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: /Unregister|取消登记/ }))
    await user.click(screen.getByRole('button', { name: /Generate exact deletion preview|生成精确删除预览/ }))
    const confirmation = await screen.findByRole('textbox')
    await user.type(confirmation, appA.slug)
    await user.click(screen.getByRole('button', { name: /Confirm unregistration|确认取消登记/ }))

    window.location.hash = `#/apps/${appB.id}`
    window.dispatchEvent(new HashChangeEvent('hashchange'))
    expect(await screen.findByText('App B')).toBeTruthy()
    resolveDelete?.(new Response('{}', { status: 200 }))
    await Promise.resolve()
    expect(window.location.hash).toBe(`#/apps/${appB.id}`)
  })

  it('does not send an A webhook mutation when its digest resolves after switching to B', async () => {
    const appA = configuredApp('00000000-0000-4000-8000-000000000051', 'App A')
    const appB = appDetail('00000000-0000-4000-8000-000000000052', 'App B')
    let resolveDigest: ((value: ArrayBuffer) => void) | undefined
    const digest = new Promise<ArrayBuffer>((resolve) => { resolveDigest = resolve })
    const webhookMutations: string[] = []
    const originalCrypto = globalThis.crypto
    vi.stubGlobal('crypto', {
      getRandomValues: originalCrypto.getRandomValues.bind(originalCrypto),
      randomUUID: originalCrypto.randomUUID.bind(originalCrypto),
      subtle: { digest: vi.fn(() => digest) },
    })
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false, addEventListener() {}, removeEventListener() {} })))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/api/v1/me')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/system/installation')) return new Response('{}', { status: 200 })
      if (url.endsWith('/api/v1/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith(`/apps/${appA.id}/webhook`)) {
        if (init?.method === 'PUT') webhookMutations.push(url)
        return new Response(JSON.stringify({ configured: false, degraded: false, metadata_revision: null, secret_revision: null, algorithm: 'hmac-sha256-v1', public_origin: 'https://example.test', public_path: '/hook', created_at: null, rotated_at: null }), { status: 200 })
      }
      if (url.endsWith(`/apps/${appB.id}/webhook`)) return new Response('{}', { status: 404 })
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith(`/apps/${appA.id}`)) return new Response(JSON.stringify(appA), { status: 200 })
      if (url.endsWith(`/apps/${appB.id}`)) return new Response(JSON.stringify(appB), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    window.location.hash = `#/apps/${appA.id}`
    auth.set({ kind: 'authenticated', me: { username: 'admin', session: { created_at: '', expires_at: '' } } })
    render(App)
    expect(await screen.findByText('App A')).toBeTruthy()
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: /Configuration|配置/ }))
    await user.click(await screen.findByRole('button', { name: /Generate webhook secret|生成 webhook secret/ }))
    await user.click(screen.getByRole('checkbox', { name: /saved this secret|已安全保存/ }))
    await user.click(screen.getByRole('button', { name: /Confirm configuration|确认配置/ }))

    window.location.hash = `#/apps/${appB.id}`
    window.dispatchEvent(new HashChangeEvent('hashchange'))
    expect(await screen.findByText('App B')).toBeTruthy()
    resolveDigest?.(new Uint8Array(32).buffer)
    await Promise.resolve()
    expect(webhookMutations).toEqual([])
  })
})
