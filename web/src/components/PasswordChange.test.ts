// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { get } from 'svelte/store'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import PasswordChange from './PasswordChange.svelte'
import { auth } from '../lib/auth'
import { setLocale } from '../lib/i18n'

const authenticated = {
  kind: 'authenticated' as const,
  me: { username: 'admin', session: { created_at: '2026-01-01T00:00:00Z', expires_at: '2026-01-02T00:00:00Z' } },
}

beforeEach(() => {
  auth.set(authenticated)
  setLocale('zh-CN', false)
})

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

async function fillPasswords(current: string, password: string, confirmation = password) {
  const user = userEvent.setup()
  await user.type(screen.getByLabelText('当前密码'), current)
  await user.type(screen.getByLabelText('新密码（14–128 个字符）'), password)
  await user.type(screen.getByLabelText('确认新密码'), confirmation)
  return user
}

describe('PasswordChange', () => {
  it('rejects mismatch and policy failures without sending a request', async () => {
    const fetch = vi.fn()
    vi.stubGlobal('fetch', fetch)
    render(PasswordChange)
    const user = await fillPasswords('current-password-canary', 'new-password-canary', 'different-password')

    await user.click(screen.getByRole('button', { name: '修改密码' }))
    expect((await screen.findByRole('alert')).textContent).toContain('两次输入的密码不一致')
    expect(fetch).not.toHaveBeenCalled()

    await user.clear(screen.getByLabelText('新密码（14–128 个字符）'))
    await user.type(screen.getByLabelText('新密码（14–128 个字符）'), 'too-short')
    await user.clear(screen.getByLabelText('确认新密码'))
    await user.type(screen.getByLabelText('确认新密码'), 'too-short')
    await user.click(screen.getByRole('button', { name: '修改密码' }))
    expect((await screen.findByRole('alert')).textContent).toContain('密码需为 14–128 个 Unicode 字符')
    expect(fetch).not.toHaveBeenCalled()
  })

  it('submits only current and new passwords without an idempotency key, then enters login', async () => {
    const fetch = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response(null, { status: 204 }))
    vi.stubGlobal('fetch', fetch)
    render(PasswordChange)
    const current = 'current-password-canary'
    const password = 'new-password-canary'
    const user = await fillPasswords(current, password)

    await user.click(screen.getByRole('button', { name: '修改密码' }))

    expect(fetch).toHaveBeenCalledOnce()
    const [url, init] = fetch.mock.calls[0]
    expect(url).toBe('/api/v1/me/password')
    expect(init?.method).toBe('PUT')
    const headers = new Headers(init?.headers)
    expect(headers.get('Idempotency-Key')).toBeNull()
    expect(JSON.parse(String(init?.body))).toEqual({
      current_password: current,
      new_password: password,
    })
    expect(get(auth)).toEqual({ kind: 'login' })
  })

  it.each([
    ['CURRENT_PASSWORD_INVALID', 'The current password is invalid', '当前密码不正确'],
    ['AUTH_COOLDOWN', 'Authentication is temporarily unavailable', '认证尝试过多'],
    ['CSRF_INVALID', 'The CSRF token is invalid', 'CSRF_INVALID: The CSRF token is invalid'],
  ])('keeps the authenticated view on deterministic %s errors', async (code, message, expected) => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      code,
      message,
      request_id: 'request-safe-42',
    }), { status: code === 'AUTH_COOLDOWN' ? 429 : 403, headers: { 'Content-Type': 'application/json' } })))
    render(PasswordChange)
    const current = 'current-password-canary'
    const password = 'new-password-canary'
    const user = await fillPasswords(current, password)

    await user.click(screen.getByRole('button', { name: '修改密码' }))

    const error = await screen.findByRole('alert')
    expect(error.textContent).toContain(expected)
    expect(error.textContent).toContain('request-safe-42')
    expect(document.body.textContent).not.toContain(current)
    expect(document.body.textContent).not.toContain(password)
    expect(get(auth)).toEqual(authenticated)
  })

  it.each([
    ['network failure', async () => { throw new TypeError('private transport detail') }],
    ['HTML proxy failure', async () => new Response('<html>private proxy detail</html>', {
      status: 502,
      headers: { 'Content-Type': 'text/html', 'X-Request-ID': 'proxy-request-42' },
    })],
    ['unexpected HTTP 200', async () => new Response(JSON.stringify({ private: 'protocol detail' }), {
      status: 200,
      headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'protocol-request-200' },
    })],
    ['unexpected HTTP 202', async () => new Response(JSON.stringify({ private: 'protocol detail' }), {
      status: 202,
      headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'protocol-request-202' },
    })],
  ])('reports an unconfirmed %s once without retrying or exposing secrets', async (_name, response) => {
    const fetch = vi.fn(response)
    vi.stubGlobal('fetch', fetch)
    render(PasswordChange)
    const current = 'current-password-canary'
    const password = 'new-password-canary'
    const user = await fillPasswords(current, password)

    await user.click(screen.getByRole('button', { name: '修改密码' }))

    const error = await screen.findByRole('alert')
    expect(error.textContent).toContain('无法确认密码修改结果')
    expect(error.textContent).not.toContain('private')
    expect(document.body.textContent).not.toContain(current)
    expect(document.body.textContent).not.toContain(password)
    expect(fetch).toHaveBeenCalledOnce()
    expect(get(auth)).toEqual(authenticated)
  })

  it('uses the common 401 transition without treating it as a wrong current password', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('<html>expired</html>', {
      status: 401,
      headers: { 'Content-Type': 'text/html' },
    })))
    render(PasswordChange)
    const user = await fillPasswords('current-password-canary', 'new-password-canary')

    await user.click(screen.getByRole('button', { name: '修改密码' }))

    expect(get(auth)).toEqual({ kind: 'login' })
    expect((await screen.findByRole('alert')).textContent).not.toContain('当前密码不正确')
  })
})
