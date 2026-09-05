// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import StorageCleanup from './StorageCleanup.svelte'

const preview = {
  candidates: [{
    artifact_kind: 'release', app_id: '00000000-0000-4000-8000-000000000001',
    artifact_id: '00000000-0000-4000-8000-000000000002',
    estimated_logical_bytes: 128, release_created_at: '2026-01-01T00:00:00Z',
  }],
  protected: [{ reason: 'active', count: 1 }],
  estimated_logical_bytes: 128,
  confirmation_token: 'write-only-token',
  expires_at: '2026-01-01T00:05:00Z',
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('StorageCleanup', () => {
  it('does not request on mount and requires a preview plus acknowledgement', async () => {
    const fetch = vi.fn(async () => new Response(JSON.stringify(preview), { status: 200 }))
    vi.stubGlobal('fetch', fetch)
    render(StorageCleanup)
    expect(fetch).not.toHaveBeenCalled()
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '扫描可清理存储' }))
    expect(await screen.findByText(/00000000-0000-4000-8000-000000000002/)).toBeTruthy()
    expect((screen.getByRole('button', { name: '应用精确清理计划' }) as HTMLButtonElement).disabled).toBe(true)
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('freezes the preview token and reuses the exact body and key only after an unknown result', async () => {
    const requests: Array<{ body: string; key: string }> = []
    let apply = 0
    const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).endsWith('/preview')) return new Response(JSON.stringify(preview), { status: 200 })
      requests.push({
        body: String(init?.body),
        key: new Headers(init?.headers).get('Idempotency-Key') ?? '',
      })
      apply += 1
      if (apply === 1) throw new TypeError('response lost')
      return new Response(JSON.stringify({
        operation_id: '00000000-0000-4000-8000-000000000005',
        plan_hash: 'a'.repeat(64), status: 'completed', idempotency_replayed: true,
        items: [{ app_id: preview.candidates[0].app_id, artifact_kind: 'release', artifact_id: preview.candidates[0].artifact_id, status: 'deleted' }],
      }), { status: 200 })
    })
    vi.stubGlobal('fetch', fetch)
    vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000006' })
    render(StorageCleanup)
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '扫描可清理存储' }))
    await user.click(await screen.findByRole('checkbox'))
    await user.click(screen.getByRole('button', { name: '应用精确清理计划' }))
    expect((await screen.findByRole('alert')).textContent).toContain('无法确认清理结果')
    expect((screen.getByRole('button', { name: '重新扫描' }) as HTMLButtonElement).disabled).toBe(true)
    await user.click(screen.getByRole('button', { name: '确认同一清理结果' }))
    expect(await screen.findByText('清理已完成。')).toBeTruthy()
    expect(requests[0]).toEqual(requests[1])
    expect(requests[0].body).toContain('write-only-token')
    expect(document.body.textContent).not.toContain('write-only-token')
  })

  it('clears the old preview after a known stale response', async () => {
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      if (String(input).endsWith('/preview')) return new Response(JSON.stringify(preview), { status: 200 })
      return new Response(JSON.stringify({ code: 'CLEANUP_PREVIEW_STALE', message: 'safe', request_id: 'request-1' }), {
        status: 409, headers: { 'Content-Type': 'application/json' },
      })
    }))
    vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000007' })
    render(StorageCleanup)
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '扫描可清理存储' }))
    await user.click(await screen.findByRole('checkbox'))
    await user.click(screen.getByRole('button', { name: '应用精确清理计划' }))
    expect((await screen.findByRole('alert')).textContent).toContain('请重新扫描')
    expect(screen.queryByText(/00000000-0000-4000-8000-000000000002/)).toBeNull()
  })

  it('shows confirmed partial results and requires a fresh scan', async () => {
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      if (String(input).endsWith('/preview')) return new Response(JSON.stringify(preview), { status: 200 })
      return new Response(JSON.stringify({
        operation_id: '00000000-0000-4000-8000-000000000008',
        plan_hash: 'a'.repeat(64), status: 'completed_with_failures', idempotency_replayed: false,
        items: [{
          app_id: preview.candidates[0].app_id,
          artifact_kind: 'release',
          artifact_id: preview.candidates[0].artifact_id,
          status: 'retained',
          error_code: 'CLEANUP_ITEM_RETAINED',
        }],
      }), { status: 200 })
    }))
    vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000009' })
    render(StorageCleanup)
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '扫描可清理存储' }))
    await user.click(await screen.findByRole('checkbox'))
    await user.click(screen.getByRole('button', { name: '应用精确清理计划' }))
    expect(await screen.findByText(/清理完成但有项目被保留/)).toBeTruthy()
    expect(screen.queryByRole('button', { name: '应用精确清理计划' })).toBeNull()
    expect(screen.getByRole('button', { name: '扫描可清理存储' })).toBeTruthy()
  })

  it('ignores a late preview response after unmount', async () => {
    let resolve!: (response: Response) => void
    vi.stubGlobal('fetch', vi.fn(() => new Promise<Response>((done) => { resolve = done })))
    const view = render(StorageCleanup)
    await userEvent.setup().click(screen.getByRole('button', { name: '扫描可清理存储' }))
    view.unmount()
    resolve(new Response(JSON.stringify(preview), { status: 200 }))
    await waitFor(() => expect(document.body.textContent).not.toContain('write-only-token'))
  })

  it.each([204, 202, 200])('retains exact retry after an unconfirmed %s response', async (status) => {
    const requests: Array<{ body: string; key: string }> = []
    const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).endsWith('/preview')) return new Response(JSON.stringify(preview), { status: 200 })
      requests.push({ body: String(init?.body), key: new Headers(init?.headers).get('Idempotency-Key') ?? '' })
      return new Response(status === 204 ? null : JSON.stringify({ internal: 'write-only-token /private/path' }), { status })
    })
    vi.stubGlobal('fetch', fetch)
    vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000006' })
    render(StorageCleanup)
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '扫描可清理存储' }))
    await user.click(await screen.findByRole('checkbox'))
    await user.click(screen.getByRole('button', { name: '应用精确清理计划' }))
    expect((await screen.findByRole('alert')).textContent).toContain('无法确认清理结果')
    expect(screen.queryByText('清理已完成。')).toBeNull()
    expect((screen.getByRole('button', { name: '重新扫描' }) as HTMLButtonElement).disabled).toBe(true)
    expect(fetch).toHaveBeenCalledTimes(2)
    await user.click(screen.getByRole('button', { name: '确认同一清理结果' }))
    expect(requests[1]).toEqual(requests[0])
    expect(document.body.textContent).not.toContain('write-only-token')
    expect(document.body.textContent).not.toContain('/private/path')
  })
})
