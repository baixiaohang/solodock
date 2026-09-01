// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import AppDetail from './AppDetail.svelte'

class MockEventSource {
  addEventListener() {}
  close() {}
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('app detail resource identity', () => {
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
})
