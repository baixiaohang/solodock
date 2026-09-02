// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/svelte'
import { afterEach, describe, expect, it } from 'vitest'

import ManagedFileEditor from './ManagedFileEditor.svelte'

afterEach(cleanup)

describe('ManagedFileEditor', () => {
  it('把嵌套目标冲突定位到服务端指定的可见行', () => {
    render(ManagedFileEditor, {
      rows: [
        {
          logicalName: 'config', targetPath: '/etc/app', sensitive: false,
          originalLogicalName: null, originalTargetPath: null, originalSensitive: false,
          storedSecret: false, removed: false, value: 'root',
        },
        {
          logicalName: 'settings', targetPath: '/etc/app/config.json', sensitive: false,
          originalLogicalName: null, originalTargetPath: null, originalSensitive: false,
          storedSecret: false, removed: false, value: 'nested',
        },
      ],
      issues: [{ path: 'files[1].target_path', code: 'FILE_TARGET_CONFLICT', message: '文件目标不能嵌套' }],
    })

    const paths = screen.getAllByLabelText('容器路径')
    expect(paths[0].getAttribute('aria-invalid')).toBeNull()
    expect(paths[1].getAttribute('aria-invalid')).toBe('true')
    expect(screen.getByText('文件目标不能嵌套')).toBeTruthy()
  })
})
