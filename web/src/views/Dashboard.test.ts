// @vitest-environment jsdom
import { cleanup, render, screen, waitFor, within } from '@testing-library/svelte'
import { afterEach, describe, expect, it, vi } from 'vitest'

import Dashboard from './Dashboard.svelte'

class MockEventSource {
  static instances: MockEventSource[] = []
  listeners = new Map<string, EventListener>()
  onopen: ((event: Event) => void) | null = null
  onerror: ((event: Event) => void) | null = null

  constructor(public url: string) {
    MockEventSource.instances.push(this)
  }

  addEventListener(type: string, listener: EventListener) {
    this.listeners.set(type, listener)
  }

  close() {}

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
  })

  it('在没有应用时保留注册入口', async () => {
    stubResponses([])
    render(Dashboard)
    expect(await screen.findByText('尚无应用')).toBeTruthy()
    expect(screen.getByRole('link', { name: '注册第一个应用' }).getAttribute('href')).toBe('#/apps/new')
  })
})
