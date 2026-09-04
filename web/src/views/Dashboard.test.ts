// @vitest-environment jsdom
import { cleanup, render, screen, waitFor, within } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import Dashboard from './Dashboard.svelte'

class MockEventSource {
  static instances: MockEventSource[] = []
  static failListenerForUrl: string | undefined
  listeners = new Map<string, EventListener>()
  onopen: ((event: Event) => void) | null = null
  onerror: ((event: Event) => void) | null = null
  closed = false

  constructor(public url: string) {
    MockEventSource.instances.push(this)
  }

  addEventListener(type: string, listener: EventListener) {
    if (this.url === MockEventSource.failListenerForUrl) throw new Error('listener registration failed')
    this.listeners.set(type, listener)
  }

  close() { this.closed = true }

  emit(type: string, data: unknown) {
    this.listeners.get(type)?.(new MessageEvent(type, { data: JSON.stringify(data) }))
  }
}

const health = {
  status: 'ok',
  docker: { status: 'ready', error_code: null, server_version: '28.0', api_version: '1.48', os: 'linux', architecture: 'amd64', observed_at: '2026-01-01T00:00:00Z' },
  recovery: { status: 'ok', issue_count: 0, issues_by_code: {} },
  memory: { total_bytes: 8589934592, available_bytes: 4294967296, used_percent: 50 },
  disk: { state: { status: 'normal', total_bytes: 4096, available_bytes: 2048, used_percent: 50 }, docker: null },
  streams: { active: 1, limit: 16 },
  projection: { status: 'ok' }, deployments: { active: 0, interrupted: 0, needs_attention: 0, limit: 1 },
  registry_credentials: { status: 'ok', count: 0 }, polling: { coordinator: { status: 'running', due: 0, inflight: 0 }, store_status: 'ok', enabled: 0, suppressed: 0, app_errors: 0 },
  webhooks: { status: 'ok', configured: 0, replay_records: 0 },
}

afterEach(() => {
  cleanup()
  MockEventSource.instances = []
  MockEventSource.failListenerForUrl = undefined
  vi.unstubAllGlobals()
})

function stubResponses(apps: unknown[]) {
  vi.stubGlobal('EventSource', MockEventSource)
  vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input)
    if (url.endsWith('/system/health')) return new Response(JSON.stringify(health), { status: 200 })
    if (url.endsWith('/apps')) return new Response(JSON.stringify({ docker_status: 'ready', observed_at: '2026-01-01T00:00:00Z', apps }), { status: 200 })
    throw new Error(`unexpected request: ${url}`)
  }))
}

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (cause: unknown) => void
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise
    reject = rejectPromise
  })
  return { promise, resolve, reject }
}

function appSummary(id: string, name: string) {
  return {
    id,
    slug: name.toLowerCase().replaceAll(' ', '-'),
    display_name: name,
    active_release: null,
    actual: { status: 'running', health: 'healthy', container_id: `container-${id}`, image_ref: null },
    drift_codes: [],
  }
}

function appsResponse(apps: unknown[]) {
  return { docker_status: 'ready', observed_at: '2026-01-01T00:00:00Z', apps }
}

describe('Dashboard', () => {
  it('用语义化表格展示应用事实与实时资源', async () => {
    stubResponses([{
      id: '00000000-0000-0000-0000-000000000001', slug: 'demo', display_name: 'Demo App',
      active_release: { id: 'release-1', image_ref: `ghcr.io/demo/app@sha256:${'a'.repeat(64)}` },
      actual: { status: 'running', health: 'healthy', container_id: 'container-1', image_ref: `ghcr.io/demo/app@sha256:${'a'.repeat(64)}` },
      drift_codes: ['IMAGE_REF_MISMATCH'],
    }])
    render(Dashboard)

    const table = await screen.findByRole('table')
    expect(within(table).getAllByRole('columnheader').map((cell) => cell.textContent)).toEqual(['应用', '状态', 'CPU', '内存', '活动镜像', 'Drift'])
    expect(screen.getByRole('link', { name: /Demo App/ }).getAttribute('href')).toBe('#/apps/00000000-0000-0000-0000-000000000001')
    expect(screen.getByText('运行中')).toBeTruthy()
    expect(screen.getByText('sha256:aaaaaaaaaaaa')).toBeTruthy()
    expect(screen.getByText('运行镜像与活动镜像不一致')).toBeTruthy()
    expect(screen.getByText('主机内存可用')).toBeTruthy()
    expect(screen.getByText('4.0 GiB')).toBeTruthy()

    await waitFor(() => expect(MockEventSource.instances).toHaveLength(1))
    MockEventSource.instances[0].emit('stats', { cpu_percent: 2.5, memory_usage_bytes: 1048576 })
    expect(await screen.findByText('2.5%')).toBeTruthy()
    expect(screen.getByText('1.0 MiB')).toBeTruthy()

    const firstSource = MockEventSource.instances[0]
    await userEvent.setup().click(screen.getByRole('button', { name: '刷新' }))
    await waitFor(() => expect(MockEventSource.instances).toHaveLength(2))
    expect(firstSource.closed).toBe(true)
    firstSource.emit('stats', { cpu_percent: 99, memory_usage_bytes: 99 })
    expect(screen.queryByText('99.0%')).toBeNull()
  })

  it('在没有应用时保留注册入口', async () => {
    stubResponses([])
    render(Dashboard)
    expect(await screen.findByText('尚无应用')).toBeTruthy()
    expect(screen.getByRole('link', { name: '注册第一个应用' }).getAttribute('href')).toBe('#/apps/new')
  })

  it('卸载后会中止 pending load，且迟到响应不会创建 SSE', async () => {
    const pendingHealth = deferred<Response>()
    const pendingApps = deferred<Response>()
    const signals: AbortSignal[] = []
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      if (init?.signal) signals.push(init.signal)
      return String(input).endsWith('/system/health') ? pendingHealth.promise : pendingApps.promise
    }))

    const view = render(Dashboard)
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2))
    view.unmount()
    expect(signals).toHaveLength(2)
    expect(signals.every((signal) => signal.aborted)).toBe(true)
    pendingHealth.resolve(new Response(JSON.stringify(health), { status: 200 }))
    pendingApps.resolve(new Response(JSON.stringify(appsResponse([appSummary('a', 'Late App')])), { status: 200 }))
    await Promise.all([pendingHealth.promise, pendingApps.promise])
    await Promise.resolve()

    expect(MockEventSource.instances).toHaveLength(0)
  })

  it('只发布最新 refresh generation，并关闭被替换的 SSE', async () => {
    const firstHealth = deferred<Response>()
    const firstApps = deferred<Response>()
    const firstSignals: AbortSignal[] = []
    let generation = 0
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (generation === 0 && url.endsWith('/system/health')) {
        if (init?.signal) firstSignals.push(init.signal)
        return firstHealth.promise
      }
      if (generation === 0 && url.endsWith('/apps')) {
        if (init?.signal) firstSignals.push(init.signal)
        generation = 1
        return firstApps.promise
      }
      if (url.endsWith('/system/health')) return new Response(JSON.stringify(health), { status: 200 })
      if (url.endsWith('/apps')) return new Response(JSON.stringify(appsResponse([appSummary('b', 'New App')])), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    render(Dashboard)
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2))
    await userEvent.setup().click(screen.getByRole('button', { name: '刷新' }))
    expect(firstSignals).toHaveLength(2)
    expect(firstSignals.every((signal) => signal.aborted)).toBe(true)
    expect(await screen.findByText('New App')).toBeTruthy()
    expect(MockEventSource.instances).toHaveLength(1)

    firstHealth.resolve(new Response(JSON.stringify(health), { status: 200 }))
    firstApps.resolve(new Response(JSON.stringify(appsResponse([appSummary('a', 'Old App')])), { status: 200 }))
    await Promise.all([firstHealth.promise, firstApps.promise])
    await Promise.resolve()

    expect(screen.queryByText('Old App')).toBeNull()
    expect(MockEventSource.instances).toHaveLength(1)
  })

  it('SSE replacement failure closes both the old and partially opened source set', async () => {
    let apps = [appSummary('old', 'Old App')]
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      if (url.endsWith('/system/health')) return new Response(JSON.stringify(health), { status: 200 })
      if (url.endsWith('/apps')) return new Response(JSON.stringify(appsResponse(apps)), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    render(Dashboard)
    expect(await screen.findByText('Old App')).toBeTruthy()
    await waitFor(() => expect(MockEventSource.instances).toHaveLength(1))
    const oldSource = MockEventSource.instances[0]

    apps = [appSummary('next', 'Next App'), appSummary('broken', 'Broken App')]
    MockEventSource.failListenerForUrl = '/api/v1/apps/broken/stats'
    await userEvent.setup().click(screen.getByRole('button', { name: '刷新' }))
    expect(await screen.findByText('无法加载只读观察数据')).toBeTruthy()

    expect(MockEventSource.instances).toHaveLength(3)
    expect(MockEventSource.instances.every((source) => source.closed)).toBe(true)
    expect(oldSource.closed).toBe(true)
  })

  it('最多只为八个实际容器建立 SSE', async () => {
    stubResponses(Array.from({ length: 10 }, (_, index) => appSummary(String(index), `App ${index}`)))
    render(Dashboard)
    await waitFor(() => expect(MockEventSource.instances).toHaveLength(8))
  })
})
