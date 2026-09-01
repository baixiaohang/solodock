// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import PresetNewApp from './PresetNewApp.svelte'

afterEach(() => { cleanup(); vi.unstubAllGlobals() })

describe('PostgreSQL quick deploy', () => {
  it('reuses the create identity after an ambiguous result and starts deployment as a separate stage', async () => {
    let createAttempts = 0
    const createKeys: string[] = []
    const createPasswords: string[] = []
    const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/apps/from-preset')) {
        createAttempts += 1
        createKeys.push(new Headers(init?.headers).get('Idempotency-Key') ?? '')
        createPasswords.push(JSON.parse(String(init?.body)).variables.password)
        if (createAttempts === 1) throw new TypeError('response lost')
        return new Response(JSON.stringify({ app: { id: '00000000-0000-4000-8000-000000000031' } }), { status: 201 })
      }
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000031')) {
        return new Response(JSON.stringify({
          draft_revision: '00000000-0000-4000-8000-000000000032', active_release: null,
          pending_release_id: null, actual_release_id: null, actual: null,
        }))
      }
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000031/deployments')) {
        return new Response(JSON.stringify({ deployment_id: '00000000-0000-4000-8000-000000000033' }), { status: 202 })
      }
      throw new Error(`unexpected request: ${url}`)
    })
    let keyCounter = 0
    vi.stubGlobal('fetch', fetch)
    vi.stubGlobal('crypto', {
      getRandomValues: (bytes: Uint8Array) => { bytes.fill(7); return bytes },
      randomUUID: () => `00000000-0000-4000-8000-${String(++keyCounter).padStart(12, '0')}`,
    })

    render(PresetNewApp)
    const user = userEvent.setup()
    const submit = screen.getByRole('button', { name: '创建并部署' })
    await user.click(submit)
    expect(await screen.findByText(/创建失败/)).toBeTruthy()
    await user.click(submit)
    await waitFor(() => expect(fetch.mock.calls.some(([url]) => String(url).endsWith('/deployments'))).toBe(true))
    expect(createKeys).toHaveLength(2)
    expect(createKeys[0]).toBe(createKeys[1])
    expect(createPasswords[0]).toBe(createPasswords[1])
    expect(keyCounter).toBe(2)
  })

  it('replays the exact deployment request after an ambiguous response even when server facts change', async () => {
    const deploymentKeys: string[] = []
    const deploymentBodies: string[] = []
    let detailRequests = 0
    let deploymentAttempts = 0
    const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith('/apps/from-preset')) {
        return new Response(JSON.stringify({ app: { id: '00000000-0000-4000-8000-000000000041' } }), { status: 201 })
      }
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000041')) {
        detailRequests += 1
        return new Response(JSON.stringify({
          draft_revision: '00000000-0000-4000-8000-000000000042',
          active_release: null,
          pending_release_id: detailRequests === 1 ? null : '00000000-0000-4000-8000-000000000099',
          actual_release_id: null,
          actual: null,
        }))
      }
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000041/deployments')) {
        deploymentAttempts += 1
        deploymentKeys.push(new Headers(init?.headers).get('Idempotency-Key') ?? '')
        deploymentBodies.push(String(init?.body))
        if (deploymentAttempts === 1) throw new TypeError('response lost after enqueue')
        return new Response(JSON.stringify({ deployment_id: '00000000-0000-4000-8000-000000000043', idempotency_replayed: true }), { status: 202 })
      }
      throw new Error(`unexpected request: ${url}`)
    })
    let keyCounter = 0
    vi.stubGlobal('fetch', fetch)
    vi.stubGlobal('crypto', {
      getRandomValues: (bytes: Uint8Array) => { bytes.fill(7); return bytes },
      randomUUID: () => `00000000-0000-4000-8000-${String(++keyCounter).padStart(12, '0')}`,
    })

    render(PresetNewApp)
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '创建并部署' }))
    expect(await screen.findByText(/服务和配置已创建/)).toBeTruthy()
    await user.click(screen.getByRole('button', { name: '继续部署' }))
    await waitFor(() => expect(deploymentAttempts).toBe(2))

    expect(detailRequests).toBe(1)
    expect(deploymentKeys[1]).toBe(deploymentKeys[0])
    expect(deploymentBodies[1]).toBe(deploymentBodies[0])
  })
})
