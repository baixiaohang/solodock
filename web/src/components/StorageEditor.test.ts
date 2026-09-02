// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it } from 'vitest'

import StorageEditor from './StorageEditor.svelte'

afterEach(cleanup)

describe('StorageEditor', () => {
  it('新建读写 bind 默认未确认，并在 ro/rw 切换后要求重新确认', async () => {
    const user = userEvent.setup()
    render(StorageEditor, { volumes: [], binds: [], allowedBindRoots: ['/srv/solodock-data'] })

    await user.click(screen.getByRole('button', { name: '添加宿主目录' }))
    let acknowledgement = screen.getByLabelText('确认读写 bind 1 不随 release 回滚') as HTMLInputElement
    expect(acknowledgement.checked).toBe(false)
    await user.click(acknowledgement)
    expect(acknowledgement.checked).toBe(true)

    await user.click(screen.getByLabelText('只读'))
    expect(screen.queryByLabelText('确认读写 bind 1 不随 release 回滚')).toBeNull()
    await user.click(screen.getByLabelText('只读'))
    acknowledgement = screen.getByLabelText('确认读写 bind 1 不随 release 回滚') as HTMLInputElement
    expect(acknowledgement.checked).toBe(false)
  })

  it('把 bind issue 定位到对应行', () => {
    render(StorageEditor, {
      volumes: [],
      binds: [{ source: '/srv/solodock-data/app', target_path: '/data', readonly: false, acknowledge_non_rollbackable: false }],
      allowedBindRoots: ['/srv/solodock-data'],
      issues: [{ path: 'binds[0].acknowledge_non_rollbackable', code: 'BIND_RW_ACK_REQUIRED', message: '请确认读写目录不随 release 回滚' }],
    })
    expect(screen.getByText('请确认读写目录不随 release 回滚')).toBeTruthy()
    expect(screen.getByLabelText('确认读写 bind 1 不随 release 回滚').getAttribute('aria-invalid')).toBe('true')
  })

  it('按 volume 类型定位真实名称字段', () => {
    render(StorageEditor, {
      volumes: [
        { kind: 'owned', logical_name: 'bad-owned', target_path: '/owned' },
        { kind: 'external', name: 'bad-external', target_path: '/external' },
      ],
      binds: [],
      allowedBindRoots: [],
      issues: [
        { path: 'volumes[0].logical_name', code: 'INVALID_VALUE', message: 'managed 名称无效' },
        { path: 'volumes[1].name', code: 'INVALID_VALUE', message: 'external 名称无效' },
      ],
    })
    expect(screen.getByDisplayValue('bad-owned').getAttribute('aria-invalid')).toBe('true')
    expect(screen.getByDisplayValue('bad-external').getAttribute('aria-invalid')).toBe('true')
    expect(screen.getByText('managed 名称无效')).toBeTruthy()
    expect(screen.getByText('external 名称无效')).toBeTruthy()
  })
})
