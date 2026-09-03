// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it } from 'vitest'

import AuthView from './AuthView.svelte'

afterEach(() => {
  cleanup()
  localStorage.clear()
})

describe('LanguageSwitcher', () => {
  it('switches the unauthenticated UI immediately and stores the selection', async () => {
    const user = userEvent.setup()
    render(AuthView, { mode: 'login' })

    expect(screen.getByRole('heading', { name: '欢迎回来' })).toBeTruthy()
    await user.click(screen.getByRole('button', { name: 'English' }))

    expect(screen.getByRole('heading', { name: 'Welcome back' })).toBeTruthy()
    expect(document.documentElement.lang).toBe('en')
    expect(localStorage.getItem('solodock.ui.locale.v1')).toBe('en')
  })

  it('retranslates an already-visible local validation error', async () => {
    const user = userEvent.setup()
    render(AuthView, { mode: 'login' })

    await user.type(screen.getByLabelText('密码（14–128 个字符）'), 'short')
    await user.click(screen.getByRole('button', { name: '登录' }))
    expect(screen.getByRole('alert').textContent).toContain('密码需为 14–128 个 Unicode 字符')

    await user.click(screen.getByRole('button', { name: 'English' }))
    expect(screen.getByRole('alert').textContent).toContain('Password must contain 14–128 Unicode characters')
  })
})
