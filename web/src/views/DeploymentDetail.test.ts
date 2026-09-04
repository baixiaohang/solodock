// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import DeploymentDetail from './DeploymentDetail.svelte'

function deployment(status: 'running' | 'failed', id = '00000000-0000-4000-8000-000000000001', availableActions: Array<'rollback'> = []) {
  return {
    id,
    app_id: '00000000-0000-4000-8000-000000000010',
    trigger: 'manual',
    requested_revision: '00000000-0000-4000-8000-000000000011',
    from_release_id: null,
    candidate_release_id: null,
    rollback_target_release_id: null,
    status,
    phase: status === 'running' ? 'deploying' : 'terminal',
    source_image_ref: null,
    manifest_digest: null,
    platform: null,
    error_code: null,
    health_result: null,
    created_at: '2026-01-01T00:00:00Z',
    started_at: null,
    completed_at: null,
    transitions: [],
    available_actions: availableActions,
    warnings: [],
  }
}

afterEach(() => {
  cleanup()
  vi.useRealTimers()
  vi.unstubAllGlobals()
  window.location.hash = ''
})

describe('DeploymentDetail polling', () => {
  it('backs off transient failures, resets after success, and stops at terminal state', async () => {
    vi.useFakeTimers()
    let attempt = 0
    vi.stubGlobal('fetch', vi.fn(async () => {
      attempt += 1
      if (attempt <= 2) throw new TypeError('temporary network failure')
      if (attempt === 3) return new Response(JSON.stringify(deployment('running')), { status: 200 })
      return new Response(JSON.stringify(deployment('failed')), { status: 200 })
    }))

    render(DeploymentDetail, { deploymentId: '00000000-0000-4000-8000-000000000001' })
    await vi.advanceTimersByTimeAsync(0)
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(screen.getByRole('alert').textContent).toContain('无法加载 deployment')

    await vi.advanceTimersByTimeAsync(999)
    expect(fetch).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(1)
    expect(fetch).toHaveBeenCalledTimes(2)

    await vi.advanceTimersByTimeAsync(1999)
    expect(fetch).toHaveBeenCalledTimes(2)
    await vi.advanceTimersByTimeAsync(1)
    expect(fetch).toHaveBeenCalledTimes(3)
    expect(screen.queryByRole('alert')).toBeNull()
    expect(screen.getByText('运行中')).toBeTruthy()

    await vi.advanceTimersByTimeAsync(999)
    expect(fetch).toHaveBeenCalledTimes(3)
    await vi.advanceTimersByTimeAsync(1)
    expect(fetch).toHaveBeenCalledTimes(4)
    expect(screen.getByText('失败')).toBeTruthy()

    await vi.advanceTimersByTimeAsync(30_000)
    expect(fetch).toHaveBeenCalledTimes(4)
  })

  it('cancels the next poll when destroyed', async () => {
    vi.useFakeTimers()
    let observedSignal: AbortSignal | undefined
    vi.stubGlobal('fetch', vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      observedSignal = init?.signal ?? undefined
      return new Response(JSON.stringify(deployment('running')), { status: 200 })
    }))

    const view = render(DeploymentDetail, { deploymentId: '00000000-0000-4000-8000-000000000001' })
    await vi.advanceTimersByTimeAsync(0)
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(1))
    view.unmount()
    expect(observedSignal?.aborted).toBe(false)

    await vi.advanceTimersByTimeAsync(30_000)
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('caps repeated transient-failure backoff at fifteen seconds', async () => {
    vi.useFakeTimers()
    let attempt = 0
    vi.stubGlobal('fetch', vi.fn(async () => {
      attempt += 1
      if (attempt <= 6) throw new TypeError('temporary network failure')
      return new Response(JSON.stringify(deployment('failed')), { status: 200 })
    }))

    render(DeploymentDetail, { deploymentId: '00000000-0000-4000-8000-000000000001' })
    await vi.advanceTimersByTimeAsync(0)
    expect(fetch).toHaveBeenCalledTimes(1)

    for (const [index, delay] of [1000, 2000, 4000, 8000, 15000, 15000].entries()) {
      await vi.advanceTimersByTimeAsync(delay - 1)
      expect(fetch).toHaveBeenCalledTimes(index + 1)
      await vi.advanceTimersByTimeAsync(1)
      expect(fetch).toHaveBeenCalledTimes(index + 2)
    }
    expect(screen.getByText('失败')).toBeTruthy()
  })

  it('aborts an in-flight request when destroyed', async () => {
    let observedSignal: AbortSignal | undefined
    let resolve!: (response: Response) => void
    const pending = new Promise<Response>((resolvePromise) => { resolve = resolvePromise })
    vi.stubGlobal('fetch', vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      observedSignal = init?.signal ?? undefined
      return pending
    }))

    const view = render(DeploymentDetail, { deploymentId: '00000000-0000-4000-8000-000000000001' })
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(1))
    view.unmount()
    expect(observedSignal?.aborted).toBe(true)
    resolve(new Response(JSON.stringify(deployment('failed')), { status: 200 }))
    await pending
    await Promise.resolve()
    expect(screen.queryByText('失败')).toBeNull()
  })

  it('replays an unknown rollback with the exact frozen endpoint, body, and key', async () => {
    const deploymentId = '00000000-0000-4000-8000-000000000001'
    const appId = '00000000-0000-4000-8000-000000000010'
    const posts: Array<{ url: string; body: string; key: string }> = []
    let appLoads = 0
    vi.stubGlobal('confirm', vi.fn(() => true))
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      if (url.endsWith(`/deployments/${deploymentId}`)) {
        return new Response(JSON.stringify(deployment('failed', deploymentId, ['rollback'])), { status: 200 })
      }
      if (url.endsWith(`/apps/${appId}`)) {
        appLoads += 1
        return new Response(JSON.stringify({
          active_release: { id: appLoads === 1 ? 'release-before' : 'release-after' },
          pending_release_id: null,
          actual_release_id: appLoads === 1 ? 'release-before' : 'release-after',
          actual: { id: appLoads === 1 ? 'container-before' : 'container-after' },
        }), { status: 200 })
      }
      if (url.endsWith(`/deployments/${deploymentId}/rollback`) && init?.method === 'POST') {
        posts.push({
          url,
          body: String(init.body),
          key: new Headers(init.headers).get('Idempotency-Key') ?? '',
        })
        if (posts.length === 1) throw new TypeError('response lost after commit')
        return new Response(JSON.stringify({ deployment_id: '00000000-0000-4000-8000-000000000099' }), { status: 202 })
      }
      throw new Error(`unexpected request: ${url}`)
    }))

    render(DeploymentDetail, { deploymentId })
    const rollback = await screen.findByRole('button', { name: /Roll back|回滚/ })
    const user = userEvent.setup()
    await user.click(rollback)
    await waitFor(() => expect(posts).toHaveLength(1))
    expect((await screen.findByRole('alert')).textContent).toContain('无法确认请求结果')

    await user.click(rollback)
    await waitFor(() => expect(posts).toHaveLength(2))

    expect(appLoads).toBe(1)
    expect(posts[1]).toEqual(posts[0])
    expect(window.location.hash).toBe('#/deployments/00000000-0000-4000-8000-000000000099')
  })
})
