// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import Credentials from './Credentials.svelte'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('registry credential form', () => {
  it('uses password controls and clears write-only input after submission', async () => {
    const requests: Array<{ url: string; init?: RequestInit }> = []
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      requests.push({ url, init })
      if (!init?.method || init.method === 'GET') return new Response('[]', { status: 200 })
      return new Response(JSON.stringify({
        id: '00000000-0000-0000-0000-000000000001', registry: 'ghcr.io', username: 'robot',
        revision: '00000000-0000-0000-0000-000000000002', created_at: '2026-01-01T00:00:00Z',
        rotated_at: '2026-01-01T00:00:00Z', referenced_by_apps: 0,
      }), { status: 201 })
    }))
    const user = userEvent.setup()
    render(Credentials)
    await user.type(screen.getByLabelText('Registry'), 'ghcr.io')
    await user.type(screen.getByLabelText('Username'), 'robot')
    const secret = screen.getByLabelText('Token / password') as HTMLInputElement
    expect(secret.type).toBe('password')
    await user.type(secret, 'component-secret-canary')
    await user.click(screen.getByRole('button', { name: '保存 credential' }))
    await waitFor(() => expect(requests.some((request) => request.init?.method === 'POST')).toBe(true))
    expect(secret.value).toBe('')
  })

  it('renders rotation as an explicit password form and never calls prompt', async () => {
    const prompt = vi.spyOn(window, 'prompt')
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify([{
      id: '00000000-0000-0000-0000-000000000001', registry: 'ghcr.io', username: 'robot',
      revision: '00000000-0000-0000-0000-000000000002', created_at: '2026-01-01T00:00:00Z',
      rotated_at: '2026-01-01T00:00:00Z', referenced_by_apps: 1,
    }]), { status: 200 })))
    const user = userEvent.setup()
    render(Credentials)
    const button = await screen.findByRole('button', { name: '轮换 secret' })
    await user.click(button)
    const replacement = screen.getByLabelText('新 token / password') as HTMLInputElement
    expect(replacement.type).toBe('password')
    expect(prompt).not.toHaveBeenCalled()
  })
})
