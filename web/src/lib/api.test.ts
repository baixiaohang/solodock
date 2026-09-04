// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'

import { ApiError, api, onUnauthorized } from './api'

afterEach(() => {
  vi.unstubAllGlobals()
  onUnauthorized(() => {})
})

async function expectApiError(response: Response): Promise<ApiError> {
  vi.stubGlobal('fetch', vi.fn(async () => response))
  try {
    await api('/api/v1/test')
    throw new Error('expected api() to reject')
  } catch (cause) {
    expect(cause).toBeInstanceOf(ApiError)
    return cause as ApiError
  }
}

describe('API error normalization', () => {
  it('handles an HTML 401 before parsing and returns a safe ApiError', async () => {
    const unauthorized = vi.fn()
    onUnauthorized(unauthorized)

    const error = await expectApiError(new Response('<html>private challenge</html>', {
      status: 401,
      headers: { 'Content-Type': 'text/html', 'X-Request-ID': 'edge-request-1' },
    }))

    expect(unauthorized).toHaveBeenCalledOnce()
    expect(error).toMatchObject({
      status: 401,
      body: { code: 'HTTP_ERROR', message: 'Request failed with HTTP status 401', request_id: 'edge-request-1' },
    })
    expect(error).not.toBeInstanceOf(SyntaxError)
    expect(JSON.stringify(error.body)).not.toContain('private challenge')
  })

  it('invokes the 401 handler before entering JSON body parsing', async () => {
    let handled = false
    const unauthorized = vi.fn(() => { handled = true })
    const json = vi.fn(async () => {
      expect(handled).toBe(true)
      return { code: 'SESSION_REQUIRED', message: 'Authentication required', request_id: 'request-401' }
    })
    const response = new Response(JSON.stringify({}), {
      status: 401,
      headers: { 'Content-Type': 'application/json' },
    })
    vi.spyOn(response, 'json').mockImplementation(json)
    onUnauthorized(unauthorized)
    vi.stubGlobal('fetch', vi.fn(async () => response))

    await expect(api('/api/v1/test')).rejects.toBeInstanceOf(ApiError)

    expect(unauthorized).toHaveBeenCalledOnce()
    expect(json).toHaveBeenCalledOnce()
  })

  it.each([
    ['empty', new Response(null, { status: 403, headers: { 'X-Request-ID': 'empty-request' } })],
    ['malformed JSON', new Response('{secret', { status: 429, headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'malformed-request' } })],
    ['wrong-shaped JSON', new Response(JSON.stringify({ code: 42, message: 'secret' }), { status: 500, headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'shape-request' } })],
    ['HTML', new Response('<b>secret</b>', { status: 502, headers: { 'Content-Type': 'text/html', 'X-Request-ID': 'html-request' } })],
  ])('normalizes a %s response without exposing its body', async (_name, response) => {
    const error = await expectApiError(response)

    expect(error.status).toBe(response.status)
    expect(error.body).toEqual({
      code: 'HTTP_ERROR',
      message: `Request failed with HTTP status ${response.status}`,
      request_id: response.headers.get('X-Request-ID'),
    })
    expect(JSON.stringify(error.body)).not.toContain('secret')
  })

  it.each(['application/json; charset=utf-8', 'application/problem+json'])('preserves a valid %s error envelope', async (contentType) => {
    const body = {
      code: 'CSRF_INVALID',
      message: 'The CSRF token is invalid',
      request_id: 'body-request',
      issues: [{ path: 'field', code: 'INVALID', message: 'Invalid field' }],
    }
    const error = await expectApiError(new Response(JSON.stringify(body), {
      status: 403,
      headers: { 'Content-Type': contentType },
    }))

    expect(error.body).toEqual(body)
  })

  it('prefers a safe header request ID and rejects unsafe or oversized IDs', async () => {
    const headerError = await expectApiError(new Response(JSON.stringify({
      code: 'RATE_LIMITED', message: 'Try later', request_id: 'body-request',
    }), {
      status: 429,
      headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'header-request' },
    }))
    expect(headerError.body.request_id).toBe('header-request')

    const bodyFallback = await expectApiError(new Response(JSON.stringify({
      code: 'RATE_LIMITED', message: 'Try later', request_id: 'body-request',
    }), {
      status: 429,
      headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'unsafe request id' },
    }))
    expect(bodyFallback.body.request_id).toBe('body-request')

    const oversized = 'a'.repeat(129)
    const unsafeError = await expectApiError(new Response(JSON.stringify({
      code: 'RATE_LIMITED', message: 'Try later', request_id: oversized,
    }), {
      status: 429,
      headers: { 'Content-Type': 'application/json', 'X-Request-ID': 'unsafe request id' },
    }))
    expect(unsafeError.body).toEqual({
      code: 'HTTP_ERROR', message: 'Request failed with HTTP status 429', request_id: '',
    })
  })
})
