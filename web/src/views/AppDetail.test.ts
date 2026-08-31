// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
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
        return new Response(JSON.stringify({ items: [], next_cursor: null }), { status: 200 })
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
  })
})
