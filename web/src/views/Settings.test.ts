// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { get } from 'svelte/store'
import { afterEach, describe, expect, it, vi } from 'vitest'

import Settings from './Settings.svelte'
import { timeSettings } from '../lib/time'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('Settings', () => {
  it('uses backend-provided timezone options and applies the saved value immediately', async () => {
    const fetch = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (!init?.method) return new Response(JSON.stringify({
        revision: '00000000-0000-4000-8000-000000000001',
        display_timezone: 'UTC',
        supported_timezones: ['UTC', 'Asia/Shanghai', 'America/New_York'],
        allowed_bind_roots: [],
      }), { status: 200 })
      expect(init.method).toBe('PUT')
      expect(JSON.parse(String(init.body)).display_timezone).toBe('Asia/Shanghai')
      return new Response(JSON.stringify({
        revision: '00000000-0000-4000-8000-000000000002',
        display_timezone: 'Asia/Shanghai',
        supported_timezones: ['UTC', 'Asia/Shanghai', 'America/New_York'],
        allowed_bind_roots: [],
      }), { status: 200 })
    })
    vi.stubGlobal('fetch', fetch)
    vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000003' })
    render(Settings)
    const select = await screen.findByLabelText('显示时区')
    expect(screen.queryByRole('textbox')).toBeNull()
    const user = userEvent.setup()
    await user.selectOptions(select, 'Asia/Shanghai')
    await user.click(screen.getByRole('button', { name: '保存系统设置' }))
    expect(await screen.findByText('已生效')).toBeTruthy()
    expect(get(timeSettings).timezone).toBe('Asia/Shanghai')
  })

  it('reuses the idempotency key after an ambiguous network result', async () => {
    const keys: string[] = []
    let putCount = 0
    const fetch = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (!init?.method) return new Response(JSON.stringify({
        revision: '00000000-0000-4000-8000-000000000001',
        display_timezone: 'UTC',
        supported_timezones: ['UTC', 'Asia/Shanghai'],
        allowed_bind_roots: [],
      }), { status: 200 })
      keys.push(new Headers(init.headers).get('Idempotency-Key') ?? '')
      putCount += 1
      if (putCount === 1) throw new TypeError('connection reset after commit')
      return new Response(JSON.stringify({
        revision: '00000000-0000-4000-8000-000000000002',
        display_timezone: 'Asia/Shanghai',
        supported_timezones: ['UTC', 'Asia/Shanghai'],
        allowed_bind_roots: [],
        idempotency_replayed: true,
      }), { status: 200 })
    })
    vi.stubGlobal('fetch', fetch)
    let sequence = 0
    vi.stubGlobal('crypto', { randomUUID: () => `00000000-0000-4000-8000-${String(++sequence).padStart(12, '0')}` })
    render(Settings)
    const user = userEvent.setup()
    await user.selectOptions(await screen.findByLabelText('显示时区'), 'Asia/Shanghai')
    await user.click(screen.getByRole('button', { name: '保存系统设置' }))
    expect((await screen.findByRole('alert')).textContent).toContain('无法确认请求结果')
    await user.click(screen.getByRole('button', { name: '保存系统设置' }))
    expect(await screen.findByText('已生效')).toBeTruthy()
    expect(keys).toEqual([keys[0], keys[0]])
    expect(sequence).toBe(1)
  })

  it('rotates a normal body key only after a trusted 4xx and retains it for ambiguous failures', async () => {
    const keys: string[] = []
    let putCount = 0
    vi.stubGlobal('fetch', vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      if (!init?.method) return new Response(JSON.stringify({
        revision: '00000000-0000-4000-8000-000000000001',
        display_timezone: 'UTC', supported_timezones: ['UTC', 'Asia/Shanghai'], allowed_bind_roots: [],
      }), { status: 200 })
      keys.push(new Headers(init.headers).get('Idempotency-Key') ?? '')
      putCount += 1
      if (putCount === 1) return new Response('<html>edge failure</html>', { status: 403, headers: { 'Content-Type': 'text/html' } })
      if (putCount === 2) return new Response(JSON.stringify({ code: 'VALIDATION_FAILED', message: 'safe rejection', request_id: 'known-422' }), { status: 422, headers: { 'Content-Type': 'application/json' } })
      if (putCount === 3) return new Response(JSON.stringify({ code: 'INTERNAL_ERROR', message: 'safe failure', request_id: 'unknown-500' }), { status: 500, headers: { 'Content-Type': 'application/json' } })
      return new Response(JSON.stringify({
        revision: '00000000-0000-4000-8000-000000000002',
        display_timezone: 'Asia/Shanghai', supported_timezones: ['UTC', 'Asia/Shanghai'], allowed_bind_roots: [],
      }), { status: 200 })
    }))
    let sequence = 0
    vi.stubGlobal('crypto', { randomUUID: () => `00000000-0000-4000-8000-${String(++sequence).padStart(12, '0')}` })
    render(Settings)
    const user = userEvent.setup()
    await user.selectOptions(await screen.findByLabelText('显示时区'), 'Asia/Shanghai')
    const save = screen.getByRole('button', { name: '保存系统设置' })

    for (let attempt = 1; attempt <= 4; attempt += 1) {
      await user.click(save)
      await waitFor(() => expect(keys).toHaveLength(attempt))
    }

    expect(keys[0]).toBe(keys[1])
    expect(keys[2]).not.toBe(keys[1])
    expect(keys[2]).toBe(keys[3])
    expect(sequence).toBe(2)
    expect(await screen.findByText('已生效')).toBeTruthy()
    expect(document.body.textContent).not.toContain('edge failure')
  })

  it('keeps administrator password rotation available when display settings fail to load', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => { throw new TypeError('offline') }))

    render(Settings)

    expect(await screen.findByText('管理员安全')).toBeTruthy()
    expect(screen.getByLabelText('当前密码')).toBeTruthy()
    expect(screen.getByRole('button', { name: '扫描可清理存储' })).toBeTruthy()
    expect(await screen.findByText('无法加载全局设置。')).toBeTruthy()
  })
})
