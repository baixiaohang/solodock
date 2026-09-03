// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import EnvironmentEditor from './EnvironmentEditor.svelte'
import { environmentRowsFromDraft } from '../lib/environmentRows'
import { setLocale } from '../lib/i18n'

afterEach(() => cleanup())

describe('EnvironmentEditor', () => {
  it('renders public and stored secret values in one editable row list without revealing secrets', async () => {
    let sequence = 0
    vi.stubGlobal('crypto', { randomUUID: () => `00000000-0000-4000-8000-${String(++sequence).padStart(12, '0')}` })
    const rows = environmentRowsFromDraft({
      public_environment: [{ key: 'LOG_LEVEL', value: 'info' }],
      secret_keys: ['TOKEN'],
    })
    render(EnvironmentEditor, { rows })
    expect((screen.getByDisplayValue('info') as HTMLInputElement).type).toBe('text')
    const secret = screen.getByPlaceholderText('已保存（留空保持）') as HTMLInputElement
    expect(secret.type).toBe('password')
    expect(secret.value).toBe('')
    expect(document.body.textContent).not.toContain('secret-value')

    const user = userEvent.setup()
    await user.type(secret, 'replacement')
    expect(secret.value).toBe('replacement')

    const key = screen.getByDisplayValue('TOKEN')
    await user.clear(key)
    await user.type(key, 'RENAMED_TOKEN')
    expect(screen.getByPlaceholderText('请输入新值')).toBeTruthy()
    expect(screen.queryByPlaceholderText('已保存（留空保持）')).toBeNull()
  })

  it('批量文本只包含普通变量，合法文本可往返且 Secret 状态保留', async () => {
    const rows = environmentRowsFromDraft({
      public_environment: [{ key: 'LOG_LEVEL', value: 'info' }],
      secret_keys: ['TOKEN'],
    })
    const user = userEvent.setup()
    render(EnvironmentEditor, { rows })

    await user.click(screen.getByRole('button', { name: '批量文本' }))
    const text = screen.getByLabelText('批量普通环境变量') as HTMLTextAreaElement
    expect(text.value).toBe('LOG_LEVEL=info')
    expect(text.value).not.toContain('TOKEN')
    expect(screen.getByDisplayValue('TOKEN')).toBeTruthy()

    await user.clear(text)
    await user.type(text, 'LOG_LEVEL=debug{enter}URL=https://example.test/?a=b')
    await user.click(screen.getByRole('button', { name: '逐行编辑' }))
    expect(screen.getByDisplayValue('debug')).toBeTruthy()
    expect(screen.getByDisplayValue('https://example.test/?a=b')).toBeTruthy()
    expect(screen.getByDisplayValue('TOKEN')).toBeTruthy()
  })

  it('批量文本解析失败时保留文本并阻止切回逐行模式', async () => {
    const user = userEvent.setup()
    render(EnvironmentEditor, {
      rows: environmentRowsFromDraft({ public_environment: [{ key: 'A', value: '1' }], secret_keys: [] }),
    })
    await user.click(screen.getByRole('button', { name: '批量文本' }))
    const text = screen.getByLabelText('批量普通环境变量') as HTMLTextAreaElement
    await user.clear(text)
    await user.type(text, 'BROKEN')
    expect(screen.getByText('第 1 行缺少 =')).toBeTruthy()
    setLocale('en', false)
    expect(await screen.findByText('Line 1 is missing =')).toBeTruthy()
    setLocale('zh-CN', false)
    await user.click(await screen.findByRole('button', { name: '逐行编辑' }))
    expect(screen.getByLabelText('批量普通环境变量')).toBeTruthy()
    expect(text.value).toBe('BROKEN')
  })
})
