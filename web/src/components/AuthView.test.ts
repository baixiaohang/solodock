// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import AuthView from './AuthView.svelte'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('AuthView password policy', () => {
  it('explains the backend 14-character boundary before sending bootstrap', async () => {
    const fetch = vi.fn()
    vi.stubGlobal('fetch', fetch)
    const user = userEvent.setup()
    render(AuthView, { mode: 'setup' })
    await user.type(screen.getByLabelText('Bootstrap token'), 'token')
    const shortUnicodePassword = '😀'.repeat(7)
    await user.type(screen.getByLabelText('密码（14–128 个字符）'), shortUnicodePassword)
    await user.type(screen.getByLabelText('确认密码'), shortUnicodePassword)
    await user.click(screen.getByRole('button', { name: '完成初始化' }))
    expect(screen.getByRole('alert').textContent).toContain('14–128')
    expect(fetch).not.toHaveBeenCalled()
  })
})
