// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/svelte'
import { afterEach, describe, expect, it } from 'vitest'
import NetworkEditor from './NetworkEditor.svelte'
import { networkDraft, networkEditorState } from '../lib/networks'

afterEach(cleanup)

describe('NetworkEditor', () => {
  it('默认启用 owned default，并即时阻止空的 external-only 配置', async () => {
    render(NetworkEditor, { ownedDefaultNetwork: true, externalNetworks: [] })
    const checkbox = screen.getByLabelText('创建应用专属默认网络') as HTMLInputElement
    expect(checkbox.checked).toBe(true)
    expect(screen.queryByRole('alert')).toBeNull()

    await fireEvent.click(checkbox)
    expect(screen.getByRole('alert').textContent).toContain('至少需要一个 external network')
    await fireEvent.click(screen.getByRole('button', { name: '添加 external network' }))
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('支持增删 alias', async () => {
    render(NetworkEditor, {
      ownedDefaultNetwork: false,
      externalNetworks: [{ name: 'shared', aliases: ['postgres'] }],
    })
    expect(screen.getByDisplayValue('postgres')).toBeTruthy()
    await fireEvent.click(screen.getByRole('button', { name: '添加 alias' }))
    expect(screen.getAllByText('Alias')).toHaveLength(2)
    await fireEvent.click(screen.getAllByRole('button', { name: '删除 alias' })[0])
    expect(screen.queryByDisplayValue('postgres')).toBeNull()
  })
})

describe('network form projection', () => {
  it('过滤 legacy marker 并回显 alias，再生成仅含 external 的新 payload', () => {
    const state = networkEditorState(true, [
      { kind: 'owned_default' },
      { kind: 'external', name: 'shared', aliases: ['postgres'] },
    ])
    expect(state).toEqual({
      ownedDefaultNetwork: true,
      externalNetworks: [{ name: 'shared', aliases: ['postgres'] }],
    })
    expect(networkDraft(state)).toEqual({
      owned_default_network: true,
      networks: [{ kind: 'external', name: 'shared', aliases: ['postgres'] }],
    })
  })
})
