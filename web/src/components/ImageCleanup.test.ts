// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import ImageCleanup from './ImageCleanup.svelte'

const id = `sha256:${'a'.repeat(64)}`
const preview = { candidates: [{ image_id: id, manifest_digest: id, platform_os: 'linux', platform_architecture: 'amd64', platform_variant: null, reported_size_bytes: 1024 }], protected_count: 3, confirmation_token: 'image-cleanup-secret-token-canary', expires_at: '2027-01-01T00:05:00Z' }
const terminal = { operation_id: '00000000-0000-4000-8000-000000000001', plan_hash: 'a'.repeat(64), status: 'completed', items: [{ image_id: id, status: 'removed' }], idempotency_replayed: false }
afterEach(() => { cleanup(); vi.unstubAllGlobals() })
const response = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } })
async function choose() {
  const user = userEvent.setup()
  await user.click(screen.getByRole('button', { name: '扫描未使用的 Docker 镜像' }))
  const boxes = await screen.findAllByRole('checkbox')
  await user.click(boxes[0]!)
  await user.click(boxes[1]!)
  return user
}
describe('ImageCleanup', () => {
  it('never scans on mount, selects no images, and requires both explicit gates', async () => {
    const fetch = vi.fn(async () => response(preview)); vi.stubGlobal('fetch', fetch)
    render(ImageCleanup); expect(fetch).not.toHaveBeenCalled()
    const user = userEvent.setup()
    await user.click(screen.getByRole('button', { name: '扫描未使用的 Docker 镜像' }))
    const boxes = await screen.findAllByRole('checkbox')
    expect(boxes.every(box => !(box as HTMLInputElement).checked)).toBe(true)
    await user.click(boxes[1]!)
    expect((screen.getByRole('button', { name: '删除选中镜像' }) as HTMLButtonElement).disabled).toBe(true)
    await user.click(boxes[1]!); await user.click(boxes[0]!)
    expect((screen.getByRole('button', { name: '删除选中镜像' }) as HTMLButtonElement).disabled).toBe(true)
    expect(fetch).toHaveBeenCalledTimes(1)
  })
  it.each(['lost', '204', '202', 'shape', 'removed-array', 'retained-array'] as const)('retains exact body/key for unconfirmed %s and never retries automatically', async kind => {
    const attempts: Array<{ body: unknown; key: string | null }> = []
    vi.stubGlobal('fetch', vi.fn(async (url: string, init: RequestInit) => {
      if (url.endsWith('/preview')) return response(preview)
      attempts.push({ body: init.body, key: new Headers(init.headers).get('Idempotency-Key') })
      if (attempts.length === 1) {
        if (kind === 'lost') throw new Error('raw-token-path-canary')
        if (kind === '204') return new Response(null, { status: 204 })
        if (kind === 'removed-array' || kind === 'retained-array') return response({ ...terminal, items: [{ image_id: id, status: [kind === 'removed-array' ? 'removed' : 'retained'], raw: 'raw-token-path-canary' }] })
        return response(kind === 'shape' ? { raw: 'raw-token-path-canary' } : terminal, kind === '202' ? 202 : 200)
      }
      return response(terminal)
    }))
    render(ImageCleanup); const user = await choose()
    await user.click(screen.getByRole('button', { name: '删除选中镜像' }))
    expect((await screen.findByRole('alert')).textContent).toContain('结果未知')
    expect(attempts).toHaveLength(1)
    expect(screen.getAllByRole('checkbox').every(box => (box as HTMLInputElement).disabled)).toBe(true)
    await user.click(screen.getByRole('button', { name: '确认同一次镜像清理' }))
    expect(await screen.findByRole('status')).toHaveProperty('textContent', '选中镜像的清理结果已确认。')
    expect(attempts[0]).toEqual(attempts[1]); expect(attempts[0]!.key).toBeTruthy()
    expect(document.body.textContent).not.toContain(preview.confirmation_token)
    expect(document.body.textContent).not.toContain('raw-token-path-canary')
  })
  it.each(['stale', 'partial'])('requires a fresh preview after %s', async kind => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => url.endsWith('/preview') ? response(preview) : kind === 'stale'
      ? response({ code: 'CLEANUP_PREVIEW_STALE', message: 'safe', request_id: 'request-1' }, 409)
      : response({ ...terminal, status: 'completed_with_failures', items: [{ image_id: id, status: 'retained' }] })))
    render(ImageCleanup); const user = await choose()
    await user.click(screen.getByRole('button', { name: '删除选中镜像' }))
    await waitFor(() => expect(screen.queryAllByRole('checkbox')).toHaveLength(0))
    expect((screen.getByRole('button', { name: '扫描未使用的 Docker 镜像' }) as HTMLButtonElement).disabled).toBe(false)
  })
  it('ignores an unmounted late preview without starting any cleanup', async () => {
    let resolve!: (value: Response) => void
    const fetch = vi.fn(() => new Promise<Response>(done => { resolve = done })); vi.stubGlobal('fetch', fetch)
    const view = render(ImageCleanup)
    await userEvent.setup().click(screen.getByRole('button', { name: '扫描未使用的 Docker 镜像' }))
    view.unmount(); resolve(response(preview))
    await Promise.resolve(); expect(fetch).toHaveBeenCalledTimes(1); expect(screen.queryByRole('checkbox')).toBeNull()
  })
})
