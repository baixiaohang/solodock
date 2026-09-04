// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent, { type UserEvent } from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import PresetNewApp from './PresetNewApp.svelte'

afterEach(() => { cleanup(); vi.unstubAllGlobals() })

function stubCrypto() {
  let keyCounter = 0
  vi.stubGlobal('crypto', {
    getRandomValues: (bytes: Uint8Array) => { bytes.fill(7); return bytes },
    randomUUID: () => `00000000-0000-4000-8000-${String(++keyCounter).padStart(12, '0')}`,
  })
  return () => keyCounter
}

async function confirmSafety(user: UserEvent) {
  await user.click(screen.getByRole('checkbox', { name: /PostgreSQL 数据不会随部署或回滚而回退/ }))
  await user.click(screen.getByRole('checkbox', { name: /已在 SoloDock 之外保存生成的 PostgreSQL 密码/ }))
}

describe('PostgreSQL quick deploy', () => {
  it('requires both independent safety confirmations before any request', async () => {
    stubCrypto()
    const fetch = vi.fn()
    vi.stubGlobal('fetch', fetch)
    render(PresetNewApp)
    const user = userEvent.setup()
    const dataConfirmation = screen.getByRole('checkbox', { name: /PostgreSQL 数据不会随部署或回滚而回退/ })
    const passwordConfirmation = screen.getByRole('checkbox', { name: /已在 SoloDock 之外保存生成的 PostgreSQL 密码/ })
    const submit = screen.getByRole('button', { name: '创建并部署' })

    expect((submit as HTMLButtonElement).disabled).toBe(true)
    await user.click(dataConfirmation)
    expect((submit as HTMLButtonElement).disabled).toBe(true)
    await user.click(submit)
    expect(fetch).not.toHaveBeenCalled()

    await user.click(dataConfirmation)
    await user.click(passwordConfirmation)
    expect((submit as HTMLButtonElement).disabled).toBe(true)
    await user.click(submit)
    expect(fetch).not.toHaveBeenCalled()
  })

  it('clears the password-saved confirmation when the password changes or regenerates', async () => {
    stubCrypto()
    render(PresetNewApp)
    const user = userEvent.setup()
    const passwordConfirmation = screen.getByRole('checkbox', { name: /已在 SoloDock 之外保存生成的 PostgreSQL 密码/ })
    const passwordInput = screen.getByLabelText(/自动生成密码/)

    await user.click(passwordConfirmation)
    expect((passwordConfirmation as HTMLInputElement).checked).toBe(true)
    await user.type(passwordInput, 'x')
    expect((passwordConfirmation as HTMLInputElement).checked).toBe(false)

    await user.click(passwordConfirmation)
    await user.click(screen.getByRole('button', { name: '重新生成' }))
    expect((passwordConfirmation as HTMLInputElement).checked).toBe(false)
  })

  it('does not infer password storage from clipboard actions or failed copies', async () => {
    stubCrypto()
    render(PresetNewApp)
    const user = userEvent.setup()
    const writeText = vi.spyOn(navigator.clipboard, 'writeText')
      .mockResolvedValueOnce(undefined)
      .mockRejectedValueOnce(new Error('clipboard unavailable'))
    const passwordConfirmation = screen.getByRole('checkbox', { name: /已在 SoloDock 之外保存生成的 PostgreSQL 密码/ })

    await user.click(screen.getByRole('button', { name: '复制密码' }))
    expect((passwordConfirmation as HTMLInputElement).checked).toBe(false)
    expect(screen.getByRole('button', { name: '已复制' })).toBeTruthy()

    await user.click(screen.getByRole('button', { name: '已复制' }))
    await waitFor(() => expect(writeText).toHaveBeenCalledTimes(2))
    expect(screen.getByRole('button', { name: '复制密码' })).toBeTruthy()
    expect((passwordConfirmation as HTMLInputElement).checked).toBe(false)
  })

  it('ignores clipboard completion after the password changes or regenerates', async () => {
    stubCrypto()
    render(PresetNewApp)
    const user = userEvent.setup()
    let resolveFirst!: () => void
    let resolveSecond!: () => void
    vi.spyOn(navigator.clipboard, 'writeText')
      .mockImplementationOnce(() => new Promise<void>((resolve) => { resolveFirst = resolve }))
      .mockImplementationOnce(() => new Promise<void>((resolve) => { resolveSecond = resolve }))
    const passwordInput = screen.getByLabelText(/自动生成密码/)

    await user.click(screen.getByRole('button', { name: '复制密码' }))
    await user.type(passwordInput, 'x')
    resolveFirst()
    await Promise.resolve()
    expect(screen.getByRole('button', { name: '复制密码' })).toBeTruthy()

    await user.click(screen.getByRole('button', { name: '复制密码' }))
    await user.click(screen.getByRole('button', { name: '重新生成' }))
    resolveSecond()
    await Promise.resolve()
    expect(screen.getByRole('button', { name: '复制密码' })).toBeTruthy()
  })

  it('does not let an older overlapping copy override the latest result', async () => {
    stubCrypto()
    render(PresetNewApp)
    const user = userEvent.setup()
    let resolveOlder!: () => void
    let rejectLatest!: (reason: Error) => void
    vi.spyOn(navigator.clipboard, 'writeText')
      .mockImplementationOnce(() => new Promise<void>((resolve) => { resolveOlder = resolve }))
      .mockImplementationOnce(() => new Promise<void>((_resolve, reject) => { rejectLatest = reject }))

    await user.click(screen.getByRole('button', { name: '复制密码' }))
    await user.click(screen.getByRole('button', { name: '复制密码' }))
    rejectLatest(new Error('latest copy failed'))
    await Promise.resolve()
    expect(screen.getByRole('button', { name: '复制密码' })).toBeTruthy()

    resolveOlder()
    await Promise.resolve()
    expect(screen.getByRole('button', { name: '复制密码' })).toBeTruthy()
  })

  it('reuses the create identity after an ambiguous result and starts deployment as a separate stage', async () => {
    let createAttempts = 0
    const createKeys: string[] = []
    const createPasswords: string[] = []
    const deploymentBodies: Array<{ acknowledge_non_rollbackable_data: boolean }> = []
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
        deploymentBodies.push(JSON.parse(String(init?.body)))
        return new Response(JSON.stringify({ deployment_id: '00000000-0000-4000-8000-000000000033' }), { status: 202 })
      }
      throw new Error(`unexpected request: ${url}`)
    })
    const keyCount = stubCrypto()
    vi.stubGlobal('fetch', fetch)

    render(PresetNewApp)
    const user = userEvent.setup()
    await confirmSafety(user)
    const submit = screen.getByRole('button', { name: '创建并部署' })
    await user.click(submit)
    expect(await screen.findByText(/创建失败/)).toBeTruthy()
    await user.click(submit)
    await waitFor(() => expect(fetch.mock.calls.some(([url]) => String(url).endsWith('/deployments'))).toBe(true))
    expect(createKeys).toHaveLength(2)
    expect(createKeys[0]).toBe(createKeys[1])
    expect(createPasswords[0]).toBe(createPasswords[1])
    expect(deploymentBodies).toEqual([{ acknowledge_non_rollbackable_data: true,
      expected_active_release_id: null,
      expected_actual_container_id: null,
      expected_actual_release_id: null,
      expected_draft_revision: '00000000-0000-4000-8000-000000000032',
      expected_pending_release_id: null,
    }])
    expect(keyCount()).toBe(2)
  })

  it('hides the password and offers application recovery after deployment is unconfirmed', async () => {
    const fetch = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      if (url.endsWith('/apps/from-preset')) {
        return new Response(JSON.stringify({ app: { id: '00000000-0000-4000-8000-000000000051' } }), { status: 201 })
      }
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000051')) {
        return new Response(JSON.stringify({
          draft_revision: '00000000-0000-4000-8000-000000000052', active_release: null,
          pending_release_id: null, actual_release_id: null, actual: null,
        }))
      }
      if (url.endsWith('/apps/00000000-0000-4000-8000-000000000051/deployments')) {
        throw new TypeError('response lost after enqueue')
      }
      throw new Error(`unexpected request: ${url}`)
    })
    stubCrypto()
    vi.stubGlobal('fetch', fetch)
    render(PresetNewApp)
    const user = userEvent.setup()
    await confirmSafety(user)
    await user.click(screen.getByRole('button', { name: '创建并部署' }))

    expect(await screen.findByText(/生成的密码不会再次显示/)).toBeTruthy()
    expect(screen.queryByLabelText(/自动生成密码/)).toBeNull()
    expect(screen.getByRole('link', { name: '进入服务详情' }).getAttribute('href')).toBe('#/apps/00000000-0000-4000-8000-000000000051')
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
    stubCrypto()
    vi.stubGlobal('fetch', fetch)

    render(PresetNewApp)
    const user = userEvent.setup()
    await confirmSafety(user)
    await user.click(screen.getByRole('button', { name: '创建并部署' }))
    expect(await screen.findByText(/服务和配置已创建/)).toBeTruthy()
    await user.click(screen.getByRole('button', { name: '继续部署' }))
    await waitFor(() => expect(deploymentAttempts).toBe(2))

    expect(detailRequests).toBe(1)
    expect(deploymentKeys[1]).toBe(deploymentKeys[0])
    expect(deploymentBodies[1]).toBe(deploymentBodies[0])
    expect(JSON.parse(deploymentBodies[0]).acknowledge_non_rollbackable_data).toBe(true)
  })
})
