// @vitest-environment jsdom
import { get } from 'svelte/store'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { auth, logout, revokeAll } from './auth'

const authenticated = {
  kind: 'authenticated' as const,
  me: { username: 'admin', session: { created_at: '2026-01-01T00:00:00Z', expires_at: '2026-01-02T00:00:00Z' } },
}

beforeEach(() => auth.set(authenticated))
afterEach(() => vi.unstubAllGlobals())

describe.each([
  ['logout', logout],
  ['revoke all', revokeAll],
] as const)('%s session action', (_name, action) => {
  it('enters login only after confirmed success', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(null, { status: 204 })))

    await action()

    expect(get(auth)).toEqual({ kind: 'login' })
  })

  it.each([
    ['network failure', async () => { throw new TypeError('offline') }],
    ['JSON CSRF rejection', async () => new Response(JSON.stringify({ code: 'CSRF_INVALID', message: 'Invalid CSRF token', request_id: 'csrf-request' }), { status: 403, headers: { 'Content-Type': 'application/json' } })],
    ['HTML WAF rejection', async () => new Response('<html>blocked</html>', { status: 403, headers: { 'Content-Type': 'text/html' } })],
    ['throttling', async () => new Response(null, { status: 429 })],
    ['server failure', async () => new Response(null, { status: 500 })],
  ])('keeps authentication on %s', async (_failure, response) => {
    vi.stubGlobal('fetch', vi.fn(response))

    await expect(action()).rejects.toBeTruthy()

    expect(get(auth)).toEqual(authenticated)
  })

  it('uses the common unauthorized transition for a 401 response', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('<html>expired</html>', {
      status: 401,
      headers: { 'Content-Type': 'text/html' },
    })))

    await expect(action()).rejects.toBeTruthy()

    expect(get(auth)).toEqual({ kind: 'login' })
  })
})
