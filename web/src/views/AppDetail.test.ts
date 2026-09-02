// @vitest-environment jsdom
import { cleanup, render, screen, waitFor, within } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import AppDetail from './AppDetail.svelte'

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

class MockEventSource {
  addEventListener() {}
  close() {}
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('app detail resource identity', () => {
  it('creates the first draft from an unconfigured service with a null revision', async () => {
    const app = {
      id: '00000000-0000-4000-8000-000000000021', slug: 'example-app', display_name: 'example-app',
      resource_names: { project_name: 'solodock-example-app', owned_default_network_name: 'solodock-example-app-default', bridge_name: 'sd-0123456789ab' },
      active_release: null, actual_release_id: null, actual: null, expected_network_plan: null,
      expected_owned_default_network: null, actual_owned_default_network: null, drift_codes: [], draft: null,
      draft_revision: null, draft_config_sha256: null, active_config_revision: null, pending_release_id: null,
      pending_image_ref: null, desired_state: 'stopped', deployment_status: 'UNCONFIGURED',
      available_actions: ['deletion_preview'], compose_available: true, polling: null,
    }
    let saved: Record<string, unknown> | undefined
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith(`/apps/${app.id}/draft`) && init?.method === 'PUT') {
        saved = JSON.parse(String(init.body)) as Record<string, unknown>
        return new Response('{}', { status: 200 })
      }
      if (url.endsWith(`/apps/${app.id}`)) return new Response(JSON.stringify(app), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    render(AppDetail, { appId: app.id })
    expect(await screen.findByText('尚未配置')).toBeTruthy()
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '配置' }))
    await user.type(screen.getByLabelText('发现镜像 tag'), 'ghcr.io/example/app:stable')
    await user.click(screen.getByRole('button', { name: '保存新 revision' }))
    await waitFor(() => expect(saved).toBeDefined())
    expect(saved?.expected_revision).toBeNull()
  })

  it('does not display an owned network or bridge for an external-only app', async () => {
    const app = {
      id: '00000000-0000-4000-8000-000000000001',
      slug: 'demo',
      display_name: 'Demo',
      resource_names: {
        project_name: 'solodock-demo',
        owned_default_network_name: 'solodock-demo-default',
        bridge_name: 'sd-demo',
      },
      active_release: null,
      actual_release_id: null,
      actual: null,
      expected_network_plan: {
        owned_default_network: false,
        mode: 'external_only',
        external: [{ name: 'shared', aliases: [] }],
      },
      expected_owned_default_network: null,
      actual_owned_default_network: null,
      drift_codes: [],
      draft: {
        discovery_image_ref: 'registry.example/app:stable',
        credential_ref: null,
        auto_deploy_enabled: false,
        poll_interval_seconds: 300,
        stop_grace_period_seconds: 10,
        public_environment: [],
        secret_keys: [],
        files: [],
        ports: [],
        volumes: [],
        binds: [],
        owned_default_network: false,
        networks: [{ kind: 'external', name: 'shared', aliases: [] }],
        health: { policy: 'running', stable_window_seconds: 15 },
      },
      draft_revision: '00000000-0000-4000-8000-000000000002',
      draft_config_sha256: 'a'.repeat(64),
      active_config_revision: null,
      pending_release_id: null,
      pending_image_ref: null,
      desired_state: 'stopped',
      deployment_status: 'DEPLOY_REQUIRED',
      available_actions: ['deploy', 'deletion_preview'],
      compose_available: true,
      polling: null,
    }
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      if (url.endsWith('/deployments?limit=20')) {
        return new Response(JSON.stringify({ items: [
          { id: '00000000-0000-4000-8000-000000000011', app_id: app.id, trigger: 'manual', status: 'succeeded', phase: 'terminal', source_image_ref: 'registry.example/app:stable', manifest_digest: null, candidate_release_id: null, error_code: null, created_at: '2026-01-01T00:00:00Z' },
          { id: '00000000-0000-4000-8000-000000000012', app_id: app.id, trigger: 'poll', status: 'failed', phase: 'terminal', source_image_ref: 'registry.example/app:stable', manifest_digest: null, candidate_release_id: null, error_code: 'HEALTH_FAILED', created_at: '2026-01-02T00:00:00Z' },
        ], next_cursor: null }), { status: 200 })
      }
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000001')) {
        return new Response(JSON.stringify(app), { status: 200 })
      }
      throw new Error(`unexpected request: ${url}`)
    }))

    render(AppDetail, { appId: app.id })
    expect(await screen.findByText('solodock-demo-app-1')).toBeTruthy()
    expect(screen.queryByText('solodock-demo-default')).toBeNull()
    expect(screen.queryByText('sd-demo')).toBeNull()
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '配置' }))
    expect((await screen.findByLabelText(/^停机宽限（秒）/)) as HTMLInputElement).toHaveProperty('value', '10')
    expect(screen.getByText('环境变量')).toBeTruthy()
    expect(screen.queryByText('编辑完整配置…')).toBeNull()
    await user.click(screen.getByRole('button', { name: '部署历史' }))
    expect(document.querySelectorAll('.deployment-history article.deployment-row')).toHaveLength(2)
    expect(screen.getAllByRole('link', { name: '查看详情' })).toHaveLength(2)
  })

  it('shows safe server validation issues at the matching field', async () => {
    const app = {
      id: '00000000-0000-4000-8000-000000000031', slug: 'broken-config', display_name: 'broken-config',
      resource_names: { project_name: 'solodock-broken-config', owned_default_network_name: 'solodock-broken-config-default', bridge_name: 'sd-abcdef012345' },
      active_release: null, actual_release_id: null, actual: null, expected_network_plan: null,
      expected_owned_default_network: null, actual_owned_default_network: null, drift_codes: [], draft: null,
      draft_revision: null, draft_config_sha256: null, active_config_revision: null, pending_release_id: null,
      pending_image_ref: null, desired_state: 'stopped', deployment_status: 'UNCONFIGURED',
      available_actions: ['deletion_preview'], compose_available: true, polling: null,
    }
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith(`/apps/${app.id}/draft`) && init?.method === 'PUT') {
        return new Response(JSON.stringify({
          code: 'CONFIG_INVALID', message: 'The draft configuration is invalid', request_id: 'req-safe-123',
          issues: [
            { path: 'discovery_image_ref', code: 'INVALID_IMAGE_REFERENCE', message: '镜像引用无效' },
            { path: 'poll_interval_seconds', code: 'OUT_OF_RANGE', message: '检查间隔无效' },
          ],
        }), { status: 422, headers: { 'Content-Type': 'application/json' } })
      }
      if (url.endsWith(`/apps/${app.id}`)) return new Response(JSON.stringify(app), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    render(AppDetail, { appId: app.id })
    const user = userEvent.setup()
    await screen.findByText('尚未配置')
    await user.click(screen.getByRole('button', { name: '配置' }))
    const image = screen.getByLabelText('发现镜像 tag')
    await user.type(image, 'not-an-image')
    await user.click(screen.getByRole('button', { name: '保存新 revision' }))
    expect(await screen.findByText(/镜像引用无效.*req-safe-123/)).toBeTruthy()
    expect(image.getAttribute('aria-invalid')).toBe('true')
    const poll = screen.getByLabelText('检查间隔（秒）')
    expect(poll.getAttribute('aria-invalid')).toBe('true')
    await user.clear(image)
    await user.type(image, 'ghcr.io/example/app:stable')
    expect(image.getAttribute('aria-invalid')).toBeNull()
    expect(poll.getAttribute('aria-invalid')).toBe('true')
    expect(screen.getByText(/检查间隔无效.*req-safe-123/)).toBeTruthy()
  })

  it('remaps hidden Secret operations and clears indexed issues on structural edits', async () => {
    const app = {
      id: '00000000-0000-4000-8000-000000000041', slug: 'indexed-errors', display_name: 'indexed-errors',
      resource_names: { project_name: 'solodock-indexed-errors', owned_default_network_name: 'solodock-indexed-errors-default', bridge_name: 'sd-fedcba987654' },
      active_release: null, actual_release_id: null, actual: null, expected_network_plan: null,
      expected_owned_default_network: null, actual_owned_default_network: null, drift_codes: [],
      draft: {
        discovery_image_ref: 'registry.example/app:stable', credential_ref: null,
        auto_deploy_enabled: false, poll_interval_seconds: 300, stop_grace_period_seconds: 10,
        public_environment: [], secret_keys: ['OLD'], files: [],
        ports: [
          { host_ip: '127.0.0.1', host_port: 3000, container_port: 3000, protocol: 'tcp' },
          { host_ip: '127.0.0.1', host_port: 4000, container_port: 4000, protocol: 'tcp' },
        ],
        volumes: [], binds: [], owned_default_network: true, service_discovery_enabled: true,
        networks: [], health: { policy: 'running', stable_window_seconds: 15 },
      },
      draft_revision: '00000000-0000-4000-8000-000000000042', draft_config_sha256: 'b'.repeat(64),
      active_config_revision: null, pending_release_id: null, pending_image_ref: null,
      desired_state: 'stopped', deployment_status: 'DEPLOY_REQUIRED',
      available_actions: ['deploy', 'deletion_preview'], compose_available: true, polling: null,
    }
    vi.stubGlobal('EventSource', MockEventSource)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/deployments?limit=20')) return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
      if (url.endsWith('/registry-credentials')) return new Response('[]', { status: 200 })
      if (url.endsWith('/settings')) return new Response(JSON.stringify(settings), { status: 200 })
      if (url.endsWith('/webhook')) return new Response('{}', { status: 404 })
      if (url.endsWith(`/apps/${app.id}/draft`) && init?.method === 'PUT') {
        return new Response(JSON.stringify({
          code: 'CONFIG_INVALID', message: 'The draft configuration is invalid', request_id: 'req-indexed',
          issues: [
            { path: 'ports[0].host_port', code: 'PORT_CONFLICT', message: '宿主端口冲突' },
            { path: 'environment.secrets[1].value', code: 'INVALID_ENV_VALUE', message: '新 Secret 值无效' },
          ],
        }), { status: 422, headers: { 'Content-Type': 'application/json' } })
      }
      if (url.endsWith(`/apps/${app.id}`)) return new Response(JSON.stringify(app), { status: 200 })
      throw new Error(`unexpected request: ${url}`)
    }))

    render(AppDetail, { appId: app.id })
    const user = userEvent.setup()
    await screen.findByText('等待首次部署')
    await user.click(screen.getByRole('button', { name: '配置' }))

    const environment = screen.getByRole('group', { name: '环境变量' })
    await user.click(within(environment).getByRole('button', { name: '删除' }))
    await user.click(within(environment).getByRole('button', { name: '＋ 添加一行' }))
    await user.type(within(environment).getByPlaceholderText('KEY'), 'NEW')
    await user.type(within(environment).getByPlaceholderText('VALUE'), 'replacement')
    await user.click(within(environment).getByRole('checkbox', { name: '敏感' }))
    await user.click(screen.getByRole('button', { name: '保存新 revision' }))

    const replacement = within(environment).getByDisplayValue('replacement')
    expect(await screen.findByText(/宿主端口冲突.*req-indexed/)).toBeTruthy()
    expect(replacement.getAttribute('aria-invalid')).toBe('true')
    const ports = screen.getByRole('group', { name: '端口' })
    expect(within(ports).getAllByLabelText('宿主端口')[0].getAttribute('aria-invalid')).toBe('true')

    await user.click(within(ports).getAllByRole('button', { name: '删除' })[0])
    expect(within(ports).getByLabelText('宿主端口').getAttribute('aria-invalid')).toBeNull()
    expect(screen.getByText(/新 Secret 值无效.*req-indexed/)).toBeTruthy()

    await user.click(within(environment).getByRole('checkbox', { name: '敏感' }))
    expect(within(environment).getByDisplayValue('replacement').getAttribute('aria-invalid')).toBeNull()
    expect(screen.queryByText(/新 Secret 值无效/)).toBeNull()
  })
})
