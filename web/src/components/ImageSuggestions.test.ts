// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import ImageSuggestions from './ImageSuggestions.svelte'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('ImageSuggestions', () => {
  it('通过统一 mutation helper 发送镜像和 credential，并展示成功建议', async () => {
    Object.defineProperty(document, 'cookie', {
      configurable: true,
      value: '__Host-solodock_csrf=csrf-token',
    })
    const requests: Array<{ url: string; init?: RequestInit }> = []
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      requests.push({ url: String(input), init })
      return new Response(JSON.stringify({
        resolved_digest: 'sha256:abc123',
        exposed_ports: [{ container_port: 3000, protocol: 'tcp' }],
        volume_targets: ['/var/lib/example'],
        has_healthcheck: true,
        user: '10001:10001',
        stop_signal: 'SIGTERM',
        warnings: [],
      }), { status: 200 })
    }))

    const user = userEvent.setup()
    render(ImageSuggestions, {
      image: 'ghcr.io/example/example-app:staging',
      credentialRef: '00000000-0000-0000-0000-000000000001',
      ports: [],
      volumes: [],
    })
    await user.click(screen.getByRole('button', { name: '读取镜像配置建议' }))

    await screen.findByText(/sha256:abc123/)
    expect(screen.getByRole('button', { name: '采用端口 3000/tcp' })).toBeTruthy()
    expect(screen.getByRole('button', { name: '采用持久目录 /var/lib/example' })).toBeTruthy()
    expect(requests).toHaveLength(1)
    const request = requests[0]
    const headers = new Headers(request.init?.headers)
    expect(request.url).toBe('/api/v1/images/inspect-config')
    expect(request.init?.method).toBe('POST')
    expect(headers.get('Content-Type')).toBe('application/json')
    expect(headers.get('X-CSRF-Token')).toBe('csrf-token')
    expect(JSON.parse(String(request.init?.body))).toEqual({
      discovery_image_ref: 'ghcr.io/example/example-app:staging',
      credential_ref: '00000000-0000-0000-0000-000000000001',
    })
  })

  it('安全展示后端错误 code 和 message，不回显请求细节', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({
      code: 'REGISTRY_CREDENTIAL_INVALID',
      message: 'The image registry request failed',
      request_id: '00000000-0000-0000-0000-000000000002',
    }), { status: 422 })))

    const user = userEvent.setup()
    render(ImageSuggestions, {
      image: 'ghcr.io/private/image:staging',
      credentialRef: '00000000-0000-0000-0000-000000000003',
      ports: [],
      volumes: [],
    })
    await user.click(screen.getByRole('button', { name: '读取镜像配置建议' }))

    await waitFor(() => {
      expect(screen.getByText('REGISTRY_CREDENTIAL_INVALID: The image registry request failed')).toBeTruthy()
    })
    expect(document.body.textContent).not.toContain('ghcr.io/private/image:staging')
    expect(document.body.textContent).not.toContain('00000000-0000-0000-0000-000000000003')
  })
})
