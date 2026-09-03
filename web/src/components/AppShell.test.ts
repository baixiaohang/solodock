// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import AppShell from './AppShell.svelte'

afterEach(cleanup)

function stubViewport(mobile: boolean) {
  const listeners = new Set<() => void>()
  vi.stubGlobal('matchMedia', vi.fn(() => ({
    matches: mobile,
    media: '(max-width: 800px)',
    onchange: null,
    addEventListener: (_type: string, listener: () => void) => listeners.add(listener),
    removeEventListener: (_type: string, listener: () => void) => listeners.delete(listener),
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => true,
  })))
}

afterEach(() => vi.unstubAllGlobals())

describe('AppShell', () => {
  it('switches the authenticated shell language immediately', async () => {
    stubViewport(false)
    const user = userEvent.setup()
    render(AppShell, { route: '#/', onRevokeAll: vi.fn(), onLogout: vi.fn() })

    await user.click(screen.getAllByRole('button', { name: 'English' })[0])

    expect(screen.getByRole('link', { name: 'Applications' })).toBeTruthy()
    expect(document.documentElement.lang).toBe('en')
  })

  it('提供三个主导航入口并按路由映射活动态', async () => {
    stubViewport(false)
    const rendered = render(AppShell, { route: '#/apps/123', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    const applications = screen.getByRole('link', { name: '应用' })
    const credentials = screen.getByRole('link', { name: 'Registry 凭据' })
    const settings = screen.getByRole('link', { name: '系统设置' })

    expect(applications.getAttribute('href')).toBe('#/')
    expect(credentials.getAttribute('href')).toBe('#/credentials')
    expect(settings.getAttribute('href')).toBe('#/settings')
    expect(applications.getAttribute('aria-current')).toBe('page')
    expect(credentials.getAttribute('aria-current')).toBeNull()
    expect(settings.getAttribute('aria-current')).toBeNull()

    await rendered.rerender({ route: '#/credentials', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    expect(applications.getAttribute('aria-current')).toBeNull()
    expect(credentials.getAttribute('aria-current')).toBe('page')

    await rendered.rerender({ route: '#/settings', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    expect(settings.getAttribute('aria-current')).toBe('page')
    expect(credentials.getAttribute('aria-current')).toBeNull()

    await rendered.rerender({ route: '#/deployments/456', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    expect(applications.getAttribute('aria-current')).toBe('page')
    expect(credentials.getAttribute('aria-current')).toBeNull()
  })

  it('可开关移动导航，并在点击路由或按 Escape 后关闭', async () => {
    stubViewport(true)
    const user = userEvent.setup()
    render(AppShell, { route: '#/', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    const toggle = screen.getByRole('button', { name: '打开主导航' })

    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    expect(screen.queryByRole('link', { name: '应用' })).toBeNull()
    await user.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    await user.click(screen.getByRole('link', { name: 'Registry 凭据' }))
    expect(toggle.getAttribute('aria-expanded')).toBe('false')

    await user.click(toggle)
    await user.keyboard('{Escape}')
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    expect(document.activeElement).toBe(toggle)
    expect(screen.queryByRole('link', { name: '应用' })).toBeNull()
  })

  it('使用关闭按钮收起移动导航后把焦点归还菜单按钮', async () => {
    stubViewport(true)
    const user = userEvent.setup()
    render(AppShell, { route: '#/', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    const toggle = screen.getByRole('button', { name: '打开主导航' })

    await user.click(toggle)
    await user.click(screen.getAllByRole('button', { name: '关闭主导航' })[0])
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    expect(document.activeElement).toBe(toggle)
  })

  it('shows authenticated installation identity with full verified details', async () => {
    stubViewport(false)
    vi.stubGlobal('fetch', vi.fn(async (input: RequestInfo | URL) => {
      if (String(input).endsWith('/system/installation')) {
        return new Response(JSON.stringify({
          channel: 'stable',
          version: '0.2.0',
          source_sha: 'abcdef0123456789abcdef0123456789abcdef01',
          package_identity: '1'.repeat(64),
        }), { status: 200 })
      }
      return new Response(JSON.stringify({ code: 'UNAVAILABLE', message: 'unavailable', request_id: 'test' }), { status: 503 })
    }))
    const user = userEvent.setup()
    render(AppShell, { route: '#/', onRevokeAll: vi.fn(), onLogout: vi.fn() })
    await user.click(screen.getAllByRole('button', { name: 'English' })[0])

    const summary = await screen.findByText('SoloDock v0.2.0 · stable · abcdef0')
    await user.click(summary)
    expect(screen.getByText('abcdef0123456789abcdef0123456789abcdef01')).toBeTruthy()
    expect(screen.getByText('1'.repeat(64))).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Copy installation identity' })).toBeTruthy()
  })

  it('falls back to a bilingual unknown identity without blocking navigation', async () => {
    stubViewport(false)
    vi.stubGlobal('fetch', vi.fn(async () => { throw new Error('offline') }))
    const user = userEvent.setup()
    render(AppShell, { route: '#/', onRevokeAll: vi.fn(), onLogout: vi.fn() })

    const summary = await screen.findByText('SoloDock · unknown')
    await user.click(summary)
    expect(screen.getByText('源码提交')).toBeTruthy()
    expect(screen.getByRole('link', { name: '应用' })).toBeTruthy()
  })
})
