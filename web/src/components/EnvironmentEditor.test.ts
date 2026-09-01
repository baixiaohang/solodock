// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'

import EnvironmentEditor from './EnvironmentEditor.svelte'
import { environmentRowsFromDraft } from '../lib/environmentRows'

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
})
