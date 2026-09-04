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
    await user.type(screen.getByLabelText('用户名'), 'robot')
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

  it('retains a write-only identity only for unknown outcomes and clears secrets after every attempt', async () => {
    const secretCanary = `runtime-secret-${crypto.randomUUID()}`
    const keys: string[] = []
    let mutationCount = 0
    vi.stubGlobal('fetch', vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (!init?.method || init.method === 'GET') return new Response('[]', { status: 200 })
      keys.push(new Headers(init.headers).get('Idempotency-Key') ?? '')
      mutationCount += 1
      if (mutationCount === 1) return new Response(`<html>${secretCanary}</html>`, { status: 403, headers: { 'Content-Type': 'text/html' } })
      if (mutationCount === 2 || mutationCount === 5) return new Response(JSON.stringify({ code: 'VALIDATION_FAILED', message: 'safe rejection', request_id: `known-${mutationCount}` }), { status: 422, headers: { 'Content-Type': 'application/json' } })
      if (mutationCount === 3) return new Response(JSON.stringify({ code: 'INTERNAL_ERROR', message: 'safe failure', request_id: 'unknown-500' }), { status: 500, headers: { 'Content-Type': 'application/json' } })
      return new Response(JSON.stringify({
        id: '00000000-0000-4000-8000-000000000001', registry: 'ghcr.io', username: 'robot',
        revision: '00000000-0000-4000-8000-000000000002', created_at: '2026-01-01T00:00:00Z',
        rotated_at: '2026-01-01T00:00:00Z', referenced_by_apps: 0,
      }), { status: 201 })
    }))
    let sequence = 0
    const originalCrypto = globalThis.crypto
    vi.stubGlobal('crypto', {
      getRandomValues: originalCrypto.getRandomValues.bind(originalCrypto),
      subtle: originalCrypto.subtle,
      randomUUID: () => `00000000-0000-4000-8000-${String(++sequence).padStart(12, '0')}`,
    })
    render(Credentials)
    const user = userEvent.setup()
    const registry = await screen.findByLabelText('Registry')
    const username = screen.getByLabelText('用户名')
    const secret = screen.getByLabelText('Token / password') as HTMLInputElement
    await user.type(registry, 'ghcr.io')
    await user.type(username, 'robot')

    for (let attempt = 1; attempt <= 4; attempt += 1) {
      await user.type(secret, secretCanary)
      await user.click(screen.getByRole('button', { name: '保存 credential' }))
      await waitFor(() => expect(keys).toHaveLength(attempt))
      expect(secret.value).toBe('')
      expect(document.body.textContent).not.toContain(secretCanary)
    }

    await user.type(registry, 'ghcr.io')
    await user.type(username, 'robot')
    await user.type(secret, secretCanary)
    await user.click(screen.getByRole('button', { name: '保存 credential' }))
    await waitFor(() => expect(keys).toHaveLength(5))

    expect(keys[0]).toBe(keys[1])
    expect(keys[2]).not.toBe(keys[1])
    expect(keys[2]).toBe(keys[3])
    expect(keys[4]).not.toBe(keys[3])
    expect(sequence).toBe(3)
    expect(secret.value).toBe('')
    expect(document.body.textContent).not.toContain(secretCanary)
    expect(localStorage.length).toBe(0)
    expect(sessionStorage.length).toBe(0)
  })

  it('binds a shared delete retry identity to the exact credential target', async () => {
    const revision = '00000000-0000-4000-8000-000000000099'
    const credentials = [
      { id: '00000000-0000-4000-8000-000000000001', registry: 'a.example', username: 'robot-a', revision, created_at: '', rotated_at: '', referenced_by_apps: 0 },
      { id: '00000000-0000-4000-8000-000000000002', registry: 'b.example', username: 'robot-b', revision, created_at: '', rotated_at: '', referenced_by_apps: 0 },
    ]
    const deletes: Array<{ url: string; key: string }> = []
    vi.stubGlobal('confirm', vi.fn(() => true))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (!init?.method || init.method === 'GET') return new Response(JSON.stringify(credentials), { status: 200 })
      deletes.push({ url, key: new Headers(init.headers).get('Idempotency-Key') ?? '' })
      throw new TypeError('response lost after commit')
    }))

    render(Credentials)
    const buttons = await screen.findAllByRole('button', { name: '删除' })
    const user = userEvent.setup()
    await user.click(buttons[0])
    await waitFor(() => expect(deletes).toHaveLength(1))
    await waitFor(() => expect((buttons[0] as HTMLButtonElement).disabled).toBe(false))
    await user.click(buttons[0])
    await waitFor(() => expect(deletes).toHaveLength(2))
    await waitFor(() => expect((buttons[1] as HTMLButtonElement).disabled).toBe(false))
    await user.click(buttons[1])
    await waitFor(() => expect(deletes).toHaveLength(3))

    expect(deletes[0].url).toContain(credentials[0].id)
    expect(deletes[0].key).toBe(deletes[1].key)
    expect(deletes[2].url).toContain(credentials[1].id)
    expect(deletes[2].key).not.toBe(deletes[1].key)
  })
})
