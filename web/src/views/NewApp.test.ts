// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import NewApp from './NewApp.svelte'

afterEach(() => { cleanup(); vi.unstubAllGlobals() })

describe('new app onboarding', () => {
  it('creates an unconfigured service from only a 20-character-capable name', async () => {
    const fetch = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (!init?.method) return new Response(JSON.stringify({ revision: 'r1', display_timezone: 'UTC', supported_timezones: ['UTC'], allowed_bind_roots: [], slug_max_length: 20, supported_mount_types: ['owned_volume', 'external_volume', 'bind'] }))
      return new Response(JSON.stringify({ app: { id: '00000000-0000-4000-8000-000000000001', slug: JSON.parse(String(init.body)).slug, config_revision: null, stop_grace_period_seconds: null, deployment_status: 'UNCONFIGURED', warnings: [] }, idempotency_replayed: false }), { status: 201 })
    })
    vi.stubGlobal('fetch', fetch); vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000002' })
    render(NewApp); const input = screen.getByLabelText(/^服务名/) as HTMLInputElement
    expect(input.maxLength).toBe(20); expect(screen.queryByText(/JSON/)).toBeNull(); expect(screen.getByText('PostgreSQL')).toBeTruthy()
    const user = userEvent.setup(); await user.type(input, 'insight-agent'); await user.click(screen.getByRole('button', { name: '创建空白服务' }))
    expect(fetch.mock.calls.filter(([, init]) => init?.method === 'POST')).toHaveLength(1)
  })
})
